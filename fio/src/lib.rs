use std::{
    any::Any,
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    ops::DerefMut,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use io_uring::{
    cqueue, squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
use slab::Slab;

use crate::{
    fs::Fd,
    task::Task,
    util::{unlikely, ErrorWithData},
};

pub mod fs;
pub mod io;
pub mod task;
pub mod time;

mod util;

const TICKS_MS: u32 = 2500;
const DEFAULT_ENTRIES: u32 = 512;
const WAIT_WANTED: usize = 16;

type BufResult<T, Buf = Vec<u8>> = (Buf, std::io::Result<T>);

enum Lifecycle {
    Pending {
        /// Index of the task which requested this IO operation.
        index: task::Index,
    },
    #[expect(dead_code)]
    Ignored(Box<dyn Any>),
    Completed(i32),
}

struct IoInner {
    uring: IoUring,
    io_lifecycle_slab: Slab<Lifecycle>,
    task_slab: Slab<Task>,
    task_queue: VecDeque<task::Index>,
    /// Index of the current task.
    task_current: task::Index,
    /// ID of the *next* task.
    task_id_next: usize,
}

impl IoInner {
    fn next_task_id(&mut self) -> usize {
        let next = self.task_id_next;
        self.task_id_next += 1;
        next
    }
}

#[derive(Copy, Clone)]
pub struct Io(&'static RefCell<IoInner>);

#[cfg(test)]
static_assertions::assert_not_impl_all!(Io: Send, Send);

impl Io {
    /// Spawns a new task.
    pub fn spawn<Fut, T>(self, fut: Fut) -> task::Handle<T>
    where
        Fut: Future<Output = T> + 'static,
        T: Any,
    {
        let inner = &mut *self.get_mut();

        let id = inner.next_task_id();
        let index = inner.task_slab.vacant_key();
        let (task, handle) = task::Task::new(self, id, index, fut);

        let actual_index = inner.task_slab.insert(task);
        assert_eq!(index, actual_index);

        // Schedule the task to be executed (without waiting for the handle to
        // be awaited).
        inner.task_queue.push_back(task::Index(actual_index));

        handle
    }

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

        let index = inner.task_current;
        let lifecycle_index = inner.io_lifecycle_slab.insert(Lifecycle::Pending { index }) as u64;
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
        match inner.io_lifecycle_slab.get(self.lifecycle_index as usize) {
            Some(Lifecycle::Completed(result)) => {
                let result = *result;
                inner
                    .io_lifecycle_slab
                    .remove(self.lifecycle_index as usize);
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
            Some(Lifecycle::Pending { .. }) => Poll::Pending,
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
    F: AsyncFnOnce(Io) -> T + 'static,
    T: Any,
{
    let uring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
        .build(DEFAULT_ENTRIES)
        .expect("must build io_uring");

    let io = {
        let inner = IoInner {
            uring,
            io_lifecycle_slab: Slab::with_capacity(DEFAULT_ENTRIES as usize),
            task_slab: Slab::with_capacity(DEFAULT_ENTRIES as usize),
            task_queue: VecDeque::with_capacity(DEFAULT_ENTRIES as usize),
            task_current: task::Index(0),
            task_id_next: 0,
        };
        // NOTE: This leak is intentional to make the `Io` type copyable. This
        // is a tradeoff we pay for a more ergonomic API. It is worth it since
        // not only runtime creation is infrequent, but they also tend to have
        // a lifetime almost as long as the program's lifetime.
        Io(Box::leak::<'static>(Box::new(RefCell::new(inner))))
    };

    let fut = f(io);
    let main_task_handle = io.spawn(fut);
    assert_eq!(main_task_handle.index.0, 0);

    // Build the context, which includes the waker.
    // For now, since this runtime doesn't need to handle scheduling (there is
    // only one task, always), I do not think there's need to implement a
    // proper waker.
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

    let wait_ts = Timespec::new().nsec(TICKS_MS * 1000 * 1000);
    let submit_args = SubmitArgs::new().timespec(&wait_ts);

    loop {
        // Run all tasks on the queue.
        'queue: loop {
            let (index, id, mut fut) = {
                let inner = &mut *io.get_mut();

                let Some(index) = inner.task_queue.pop_front() else {
                    println!("  (no more queued tasks)");
                    // There are no more tasks.
                    break 'queue;
                };
                let task = &mut inner.task_slab[index.0];
                let fut = task.fut.take().unwrap();

                inner.task_current = index;
                (index, task.id, fut)
            };

            println!("  (will poll from #{id:?})");
            let poll_result = fut.as_mut().poll(&mut cx);

            let inner = &mut *io.get_mut();
            let task = &mut inner.task_slab[index.0];
            task.fut = Some(fut);

            match poll_result {
                Poll::Ready(resolved) => {
                    if unlikely(id.is_main()) {
                        println!("    -> ready! [MAIN!!] bye!");
                        let resolved: Box<T> = resolved.downcast().unwrap();
                        return *resolved;
                    }

                    println!("    -> ready!");
                    task.value = Some(resolved);
                    if let task::Waiter::Waiting(index) = task.waiter {
                        inner.task_queue.push_back(index);
                    }
                }
                Poll::Pending => println!("    -> pending"),
            }
        }

        // Loop until there are new events.
        loop {
            let inner = &mut *io.get_mut();

            println!("  (submitting)");
            let submit_result = inner
                .uring
                .submitter()
                .submit_with_args(WAIT_WANTED, &submit_args);
            println!("    -> done");

            match submit_result {
                Ok(_) => (),
                // Timeout without new events.
                Err(error) if error.raw_os_error() == Some(libc::ETIME) => {
                    println!("  (timeout without ANY events)");
                }
                Err(error) => panic!("failed to submit: {error}"),
            }

            let mut cq = inner.uring.completion();
            println!("  (got {} CQEs)", cq.len());
            for _ in 0..cq.len() {
                let entry = cq.next().expect("should have completion entry");
                let lifecycle_index = entry.user_data();
                println!("  [completed IO-op lifecylce index: {lifecycle_index}]");
                let Some(lifecycle) = inner.io_lifecycle_slab.get_mut(lifecycle_index as usize)
                else {
                    panic!("lifecycle must exist");
                };
                match lifecycle {
                    Lifecycle::Completed(_) => unreachable!(),
                    Lifecycle::Ignored(_) => todo!(), // ???
                    Lifecycle::Pending { index } => {
                        inner.task_queue.push_back(*index);
                        *lifecycle = Lifecycle::Completed(entry.result());
                    }
                }
            }

            // If there is at least one completion entry, we may poll the
            // function again: exit this innermost loop.
            break;
        }
    }
}
