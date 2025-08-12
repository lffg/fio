use std::{
    any::Any,
    cell::RefCell,
    future::Future,
    ops::DerefMut,
    path::Path,
    pin::{pin, Pin},
    task::{Context, Poll},
};

use io_uring::{
    cqueue, squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
use slab::Slab;

use crate::{fs::Fd, util::ErrorWithData};

pub mod fs;
pub mod io;
pub mod time;

mod util;

const TICKS_MS: u32 = 10;
const DEFAULT_ENTRIES: u32 = 512;
const WAIT_WANTED: usize = 16;

type BufResult<T, Buf = Vec<u8>> = (Buf, std::io::Result<T>);

enum Lifecycle {
    Pending,
    #[expect(dead_code)]
    Ignored(Box<dyn Any>),
    Completed(i32),
}

struct IoInner {
    uring: IoUring,
    slab: Slab<Lifecycle>,
}

#[derive(Copy, Clone)]
pub struct Io(&'static RefCell<IoInner>);

#[cfg(test)]
static_assertions::assert_not_impl_all!(Io: Send, Send);

impl Io {
    /// Opens the file.
    pub async fn open(self, path: impl AsRef<Path>) -> std::io::Result<Fd> {
        self.op(crate::fs::OpenAt::at_cwd(path.as_ref()))?.await
    }

    /// Similar to read(2): attempts to read up to `buf.len()` bytes from the
    /// file descriptor into the buffer. Returns the amount of bytes read.
    pub async fn read(self, buf: Vec<u8>, fd: Fd) -> BufResult<usize> {
        match self.op(crate::io::Read { fd, buf }) {
            Ok(op) => op.await,
            Err(error) => error.unpack(|d| d.buf),
        }
    }

    /// Sleeps for the specified amount of milliseconds.
    pub async fn sleep(self, millis: u64) -> std::io::Result<()> {
        self.op(crate::time::Sleep::from_millis(millis))?.await
    }
}

/// Private utilities.
impl Io {
    fn get_mut(self) -> impl DerefMut<Target = IoInner> {
        self.0.borrow_mut()
    }

    fn op<D: UringOp>(self, data: D) -> Result<Op<D>, ErrorWithData<D>> {
        Op::new(self, data)
    }
}

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

impl<D: UringOp + Unpin> Future for Op<D> {
    type Output = D::Output;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut *self.io.get_mut();
        match inner.slab.get(self.lifecycle_index as usize) {
            Some(Lifecycle::Completed(result)) => {
                let result = *result;
                inner.slab.remove(self.lifecycle_index as usize);
                let data = {
                    // SAFETY: We're not moving `self` in this block.
                    // Also, `D` (the type of data) must be unpin.
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
pub unsafe trait UringOp: Unpin {
    type Output;

    fn as_entry(&mut self) -> squeue::Entry;
    fn into_result(self, result: i32) -> Self::Output;
}

pub fn block_on<F, T>(f: F) -> T
where
    F: AsyncFnOnce(Io) -> T,
{
    let uring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
        .build(DEFAULT_ENTRIES)
        .expect("must build io_uring");

    let io = {
        let inner = IoInner {
            uring,
            slab: Slab::with_capacity(DEFAULT_ENTRIES as usize),
        };
        // NOTE: This leak is intentional to make the `Io` type copyable. This
        // is a tradeoff we pay for a more ergonomic API. It is worth it since
        // not only runtime creation is infrequent, but they also tend to have
        // a lifetime almost as long as the program's lifetime.
        Io(Box::leak::<'static>(Box::new(RefCell::new(inner))))
    };

    let fut = f(io);
    let mut fut = pin!(fut);

    // Build the context, which includes the waker.
    // For now, since this runtime doesn't need to handle scheduling (there is
    // only one task, always), I do not think there's need to implement a
    // proper waker.
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

    let wait_ts = Timespec::new().nsec(TICKS_MS * 1000 * 1000);
    let submit_args = SubmitArgs::new().timespec(&wait_ts);

    loop {
        println!("will poll");
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(resolved) => {
                println!("resolved!");
                return resolved;
            }
            Poll::Pending => println!("not ready, will wait"),
        }

        loop {
            let inner = &mut *io.get_mut();

            let submit_result = inner
                .uring
                .submitter()
                .submit_with_args(WAIT_WANTED, &submit_args);

            match submit_result {
                Ok(_) => (),
                Err(error) if error.raw_os_error() == Some(libc::ETIME) => {
                    continue;
                }
                Err(error) => panic!("failed to submit: {error}"),
            }

            let mut cq = inner.uring.completion();
            println!("got {} CQEs", cq.len());
            for _ in 0..cq.len() {
                let entry = cq.next().expect("should have completion entry");
                let lifecycle_index = entry.user_data();
                println!("completed index: {lifecycle_index}");
                match inner.slab.get_mut(lifecycle_index as usize) {
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
