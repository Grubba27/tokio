use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::Handle;
use crate::task::{JoinError, JoinHandle, LocalSet};
use crate::util::IdleNotifiedSet;

/// A collection of tasks spawned on a Tokio runtime.
///
/// All of the tasks must have the same return type `T`.
///
/// When the `TaskSet` is dropped, all tasks in the `TaskSet` are immediately aborted.
///
/// # Examples
///
/// Spawn multiple tasks and wait for them.
///
/// ```
/// use tokio::task::TaskSet;
///
/// #[tokio::main]
/// async fn main() {
///     let mut set = TaskSet::new();
///
///     for i in 0..10 {
///         set.spawn(async move { i });
///     }
///
///     let mut seen = [false; 10];
///     while let Some(res) = set.join_one().await.unwrap() {
///         seen[res] = true;
///     }
///
///     for i in 0..10 {
///         assert!(seen[i]);
///     }
/// }
/// ```
pub struct TaskSet<T> {
    inner: IdleNotifiedSet<JoinHandle<T>>,
}

impl<T> TaskSet<T> {
    /// Create a new `TaskSet`.
    pub fn new() -> Self {
        Self {
            inner: IdleNotifiedSet::new(),
        }
    }

    /// Returns the number of tasks currently in the `TaskSet`.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns whether the `TaskSet` is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<T: 'static> TaskSet<T> {
    /// Spawn the provided task on the task set.
    ///
    /// # Panics
    ///
    /// This method panics if called outside of a Tokio runtime.
    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = T>,
        F: Send + 'static,
        T: Send,
    {
        self.insert(crate::spawn(task));
    }

    /// Spawn the provided task on the provided runtime and store it in this `TaskSet`.
    pub fn spawn_on<F>(&mut self, task: F, handle: &Handle)
    where
        F: Future<Output = T>,
        F: Send + 'static,
        T: Send,
    {
        self.insert(handle.spawn(task));
    }

    /// Spawn the provided task on the current [`LocalSet`] and store it in this `TaskSet`.
    ///
    /// # Panics
    ///
    /// This method panics if it is called outside of a `LocalSet`.
    ///
    /// [`LocalSet`]: crate::task::LocalSet
    pub fn spawn_local<F>(&mut self, task: F)
    where
        F: Future<Output = T>,
        F: 'static,
    {
        self.insert(crate::task::spawn_local(task));
    }

    /// Spawn the provided task on the provided [`LocalSet`] and store it in this `TaskSet`.
    ///
    /// [`LocalSet`]: crate::task::LocalSet
    pub fn spawn_local_on<F>(&mut self, task: F, local_set: &LocalSet)
    where
        F: Future<Output = T>,
        F: 'static,
    {
        self.insert(local_set.spawn_local(task));
    }

    fn insert(&mut self, jh: JoinHandle<T>) {
        let mut entry = self.inner.insert_idle(jh);

        // Set the waker that is notified when the task completes.
        entry.with_value_and_context(|jh, ctx| jh.set_join_waker(ctx.waker()));
    }

    /// Wait until one of the tasks in the set completes and returns its output.
    ///
    /// Returns `None` if the set is empty.
    ///
    /// # Cancel Safety
    ///
    /// This method is cancel safe. If `join_one` is used as the event in a `tokio::select!`
    /// statement and some other branch completes first, it is guaranteed that no tasks were
    /// removed from this `TaskSet`.
    pub async fn join_one(&mut self) -> Result<Option<T>, JoinError> {
        crate::future::poll_fn(|cx| self.poll_join_one(cx)).await
    }

    /// Poll for one of the tasks in the set to complete.
    ///
    /// If this returns `Poll::Ready(Some(_))`, then the task that completed is removed from the
    /// set.
    ///
    /// When the method returns `Poll::Pending`, the `Waker` in the provided `Context` is scheduled
    /// to receive a wakeup when a task in the `TaskSet` completes. Note that on multiple calls to
    /// `poll_join_one`, only the `Waker` from the `Context` passed to the most recent call is
    /// scheduled to receive a wakeup.
    ///
    /// # Return value
    ///
    /// This function returns:
    ///
    ///  * `Poll::Pending` if the `TaskSet` is not empty but there is no task whose output is
    ///     available right now.
    ///  * `Poll::Ready(Ok(Some(value)))` if one of the tasks in this `TaskSet` has completed. The
    ///    `value` is the return value of one of the tasks that completed.
    ///  * `Poll::Ready(Err(err))` if one of the tasks in this `TaskSet` has panicked or been
    ///     aborted.
    ///  * `Poll::Ready(Ok(None))` if the `TaskSet` is empty.
    ///
    /// Note that this method may return `Poll::Pending` even if one of the tasks has completed.
    /// This can happen if the coop budget is reached.
    pub fn poll_join_one(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<T>, JoinError>> {
        // The call to `pop_notified` moves the entry to the `idle` list. It is moved back to
        // the `notified` list if the waker is notified in the `poll` call below.
        let mut entry = match self.inner.pop_notified(cx.waker()) {
            Some(entry) => entry,
            None => {
                if self.is_empty() {
                    return Poll::Ready(Ok(None));
                } else {
                    // The waker was set by `pop_notified`.
                    return Poll::Pending;
                }
            }
        };

        let res = entry.with_value_and_context(|jh, ctx| Pin::new(jh).poll(ctx));

        if let Poll::Ready(res) = res {
            entry.remove();
            Poll::Ready(Some(res).transpose())
        } else {
            // A JoinHandle generally wont emit a wakeup without being ready unless
            // the coop limit has been reached. We yield to the executor in this
            // case.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl<T> Drop for TaskSet<T> {
    fn drop(&mut self) {
        self.inner.drain(|join_handle| join_handle.abort());
    }
}

impl<T> fmt::Debug for TaskSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSet").field("len", &self.len()).finish()
    }
}

impl<T> Default for TaskSet<T> {
    fn default() -> Self {
        Self::new()
    }
}
