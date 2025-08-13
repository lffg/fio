use std::{
    any::Any,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::Io;

/// A unique ID generated per task.
///
/// N.B. not to be confused with the task's [`Index`].
#[derive(Copy, Clone, Debug)]
pub struct Id(pub(crate) usize);

impl Id {
    pub(crate) fn is_main(self) -> bool {
        self.0 == 0
    }
}

/// An index to refer to the task in the slab. This is not unique, unlike the
/// task's [`Id`].
#[derive(Copy, Clone, Debug)]
pub(crate) struct Index(pub(crate) usize);

type AnyFuture = Pin<Box<dyn Future<Output = Box<dyn Any>>>>;

pub(crate) struct Task {
    pub id: Id,
    #[expect(dead_code)]
    pub index: Index,
    pub fut: Option<AnyFuture>,
    /// The value produced by this task. Is `None` if the task didn't complete.
    pub value: Option<Box<dyn Any>>,
    /// Reference to the (other) task awaiting the completion of this task.
    pub waiter: Waiter,
}

pub(crate) enum Waiter {
    Unbound,
    Waiting(Index),
    Dropped,
}

impl Task {
    pub fn new<Fut, T>(io: Io, id: usize, index: usize, fut: Fut) -> (Task, Handle<T>)
    where
        Fut: Future<Output = T> + 'static,
        T: Any,
    {
        let id = Id(id);
        let index = Index(index);
        let fut: Option<AnyFuture> = Some(Box::pin(async move {
            let out = fut.await;
            Box::new(out) as Box<dyn Any>
        }));
        let task = Task {
            id,
            index,
            fut,
            value: None,
            waiter: Waiter::Unbound,
        };
        let handle = Handle {
            io,
            index,
            _t: PhantomData,
        };
        (task, handle)
    }
}

pub struct Handle<T> {
    pub(crate) io: Io,
    /// Index of the corresponding task.
    pub(crate) index: Index,
    _t: PhantomData<T>,
}

impl<T: Any> Future for Handle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let i = self.index.0;
        let inner = &mut *self.io.get_mut();
        let task = &mut inner.task_slab[i];

        if let Some(value) = task.value.take() {
            inner.task_slab.remove(i);
            let value: Box<T> = value.downcast().expect("type magic havoc");
            return Poll::Ready(*value);
        } else if let Waiter::Unbound = task.waiter {
            // Register the index of the *current task* (the one which executed
            // `handle.await`) such that it can be woken when this *handle's
            // task* finishes.
            task.waiter = Waiter::Waiting(inner.task_current);
        }
        Poll::Pending
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        let i = self.index.0;
        let inner = &mut *self.io.get_mut();
        let Some(task) = &mut inner.task_slab.get_mut(i) else {
            return;
        };

        if task.value.is_some() {
            inner.task_slab.remove(i);
        } else if let Waiter::Unbound = task.waiter {
            task.waiter = Waiter::Dropped;
        }
    }
}
