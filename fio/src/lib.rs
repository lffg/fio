use std::{
    any::Any, cell::RefCell, future::Future, io, ops::DerefMut, path::Path, pin::pin, task::Poll,
};

use io_uring::{
    cqueue, squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
use slab::Slab;

type BufResult<T, Buf = Vec<u8>> = (Buf, io::Result<T>);

mod util;

enum Lifecycle {
    Pending,
    #[expect(dead_code)]
    Ignored(Box<dyn Any>),
    Completed(i32),
}

#[derive(Copy, Clone)]
pub struct Fd(i32);

struct IoInner {
    uring: IoUring,
    slab: Slab<Lifecycle>,
}

#[derive(Copy, Clone)]
pub struct Io(&'static RefCell<IoInner>);

#[cfg(test)]
static_assertions::assert_not_impl_all!(Io: Send, Send);

impl Io {
    fn get_mut(self) -> impl DerefMut<Target = IoInner> {
        self.0.borrow_mut()
    }

    /// Opens the file.
    pub async fn open(self, path: impl AsRef<Path>) -> io::Result<Fd> {
        op::Op::new(self, op::OpenAt::at_cwd(path.as_ref()))?.await
    }

    /// Similar to read(2): attempts to read up to `buf.len()` bytes from the
    /// file descriptor into the buffer. Returns the amount of bytes read.
    pub async fn read(self, buf: Vec<u8>, fd: Fd) -> BufResult<usize> {
        match op::Op::new(self, op::Read { fd, buf }) {
            Ok(op) => op.await,
            Err(error) => error.unpack(|d| d.buf),
        }
    }
}

struct ErrorWithData<D>(io::Error, D);

impl<D> ErrorWithData<D> {
    #[inline]
    pub fn unpack<T, Ok>(self, f: impl FnOnce(D) -> T) -> (T, io::Result<Ok>) {
        (f(self.1), Err(self.0))
    }
}

impl<D> From<ErrorWithData<D>> for io::Error {
    fn from(value: ErrorWithData<D>) -> Self {
        value.0
    }
}

mod op {
    use std::{
        ffi::CString,
        future::Future,
        io,
        path::Path,
        pin::Pin,
        task::{Context, Poll},
    };

    use io_uring::{opcode, squeue, types};

    use crate::{
        util::{into_c_string, map_buf_result, map_result},
        BufResult, ErrorWithData, Io, Lifecycle,
    };

    pub struct Op<D> {
        io: Io,
        data: Option<D>,
        lifecycle_index: u64,
    }

    impl<D: UringOp> Op<D> {
        pub fn new(io: Io, mut data: D) -> Result<Self, ErrorWithData<D>> {
            let mut inner = io.get_mut();

            let is_full = inner.uring.submission().is_full();
            if is_full {
                if let Err(error) = inner.uring.submit() {
                    return Err(ErrorWithData(error, data));
                }
            }

            let lifecycle_index = inner.slab.insert(Lifecycle::Pending) as u64;
            let entry = data.as_entry().user_data(lifecycle_index);

            let push_result = unsafe { inner.uring.submission().push(&entry) };
            // This should not fail: we submitted above if queue was full.
            assert!(push_result.is_ok());

            Ok(Op {
                io,
                data: Some(data),
                lifecycle_index,
            })
        }
    }

    impl<D: UringOp> Future for Op<D> {
        type Output = D::Output;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            let inner = &mut *self.io.get_mut();
            match inner.slab.get(self.lifecycle_index as usize) {
                Some(Lifecycle::Completed(result)) => {
                    let result = *result;
                    inner.slab.remove(self.lifecycle_index as usize);
                    let data = {
                        // SAFETY: We're not moving `self` in this block.
                        let this = unsafe { self.get_unchecked_mut() };
                        this.data
                            .take()
                            .expect("data must exist for completed lifecycle")
                    };
                    Poll::Ready(D::into_result(data, result))
                }
                Some(Lifecycle::Ignored(_)) => todo!(), // ???
                Some(Lifecycle::Pending) => Poll::Pending,
                None => unreachable!("lifecycle must exist"),
            }
        }
    }

    /// # Safety
    ///
    /// - Implementors must ensure correct op-to-entry and entry-to-op
    ///   conversions.
    /// - Even though `as_entry` receives a `&mut self`, implementors MUST NOT
    ///   move the value pointed by `&mut self`.
    pub unsafe trait UringOp {
        type Output;

        fn as_entry(&mut self) -> squeue::Entry;
        fn into_result(self, result: i32) -> Self::Output;
    }

    pub struct OpenAt {
        path: CString,
    }

    impl OpenAt {
        pub fn at_cwd(path: &Path) -> Self {
            OpenAt {
                path: into_c_string(path.as_os_str()),
            }
        }
    }

    unsafe impl UringOp for OpenAt {
        type Output = io::Result<crate::Fd>;

        fn as_entry(&mut self) -> squeue::Entry {
            opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), self.path.as_ptr()).build()
        }

        fn into_result(self, result: i32) -> Self::Output {
            map_result(result, |fd| crate::Fd(fd))
        }
    }

    pub struct Read {
        pub buf: Vec<u8>,
        pub fd: crate::Fd,
    }

    unsafe impl UringOp for Read {
        type Output = BufResult<usize>;

        fn as_entry(&mut self) -> squeue::Entry {
            opcode::Read::new(
                types::Fd(self.fd.0),
                self.buf.as_mut_ptr(),
                u32::try_from(self.buf.len()).unwrap(),
            )
            .build()
        }

        fn into_result(self, result: i32) -> Self::Output {
            // TODO: Handle results greater than 2^31 - 1
            map_buf_result(result, self.buf, |n| n as usize)
        }
    }
}

pub fn block_on<F, T>(f: F) -> T
where
    F: AsyncFnOnce(Io) -> T,
{
    const DEFAULT_ENTRIES: u32 = 512;
    const WAIT_WANTED: usize = 16;

    let uring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
        .build(DEFAULT_ENTRIES)
        .expect("must build io_uring");

    let io = {
        let inner = IoInner {
            uring,
            slab: Slab::with_capacity(DEFAULT_ENTRIES as usize),
        };
        Io(Box::leak::<'static>(Box::new(RefCell::new(inner))))
    };

    let fut = f(io);
    let mut fut = pin!(fut);

    // Build the context, which includes the waker.
    // For now, since this runtime doesn't need to handle scheduling (there is
    // only one task, always), I do not think there's need to implement a
    // proper waker.
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

    // 5 ms = 5^3 us = 5^6 ns
    let wait_ts = Timespec::new().nsec(5 * 1000 * 1000);
    let submit_args = SubmitArgs::new().timespec(&wait_ts);

    loop {
        println!("will poll");
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(resolved) => {
                println!("resolved!");
                return resolved;
            }
            Poll::Pending => println!("not ready"),
        }

        loop {
            println!("waiting...");
            let inner = &mut *io.get_mut();

            let n = inner
                .uring
                .submitter()
                .submit_with_args(WAIT_WANTED, &submit_args)
                .expect("must be able to submit");
            if n == 0 {
                continue;
            }
            println!("got {n} CQEs");
            let mut cq = inner.uring.completion();
            for _ in 0..n {
                let entry = cq.next().expect("should have completion entry");
                match inner.slab.get_mut(entry.user_data() as usize) {
                    Some(Lifecycle::Completed(_)) => unreachable!(),
                    Some(Lifecycle::Ignored(_)) => todo!(), // ???
                    Some(it @ Lifecycle::Pending) => {
                        *it = Lifecycle::Completed(entry.result());
                    }
                    None => unreachable!("lifecycle must exist"),
                }
            }

            // If there is at least one completion entry, we may poll the
            // function again: exit this innermost loop.
            break;
        }
    }
}
