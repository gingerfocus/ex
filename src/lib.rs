#![doc = include_str!("../README.md")]
#![warn(
    // missing_docs,
    missing_debug_implementations,
    rust_2018_idioms,
    clippy::undocumented_unsafe_blocks
)]

// pub use futures_lite::future as futures;
pub mod futures;

use concurrent_queue::ConcurrentQueue;
use core::fmt;
use core::marker::PhantomData;
use core::panic::{RefUnwindSafe, UnwindSafe};
use core::task::Waker;
use std::sync::Mutex;

use futures::{Future, FutureExt};

use async_task::{Builder, Runnable};
use slab::Slab;

#[doc(no_inline)]
pub use async_task::{FallibleTask, Task};

/// An async executor.
///
/// Cannot be sent between thread and so you should create one for each thread
/// your program needs.
///
/// The use case of this is for large finite parelle work. i.e. try using
/// threads and if that fails use this and when this fails use a gpu.
///
/// # Examples
///
/// Basic usage:
/// ```
/// use async_channel::unbounded;
/// use ex::Executor;
///
/// let ex = Executor::new();
/// let (signal, shutdown) = unbounded::<()>();
///
/// for i in 0..4 {
///     ex.spawn(async {
///         shutdown.recv().await;
///         println!("Task {i}!");
///     }).detatch();
/// }
/// println!("Hello world!");
/// drop(signal);
/// while ex.try_tick() {}
/// ```
pub struct Executor<'a> {
    /// The executor state.
    state: Box<State>,

    /// Makes the `'a` lifetime invariant.
    _marker: PhantomData<&'a ()>,
}

impl UnwindSafe for Executor<'_> {}
impl RefUnwindSafe for Executor<'_> {}

impl fmt::Debug for Executor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor")
            .field("active", &self.state.active.lock().unwrap().len())
            .field("global_tasks", &self.state.queue)
            .finish()
    }
}

impl<'a> Executor<'a> {
    /// Creates a new executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use ex::Executor;
    ///
    /// let ex = Executor::new();
    /// ```
    pub fn new() -> Executor<'a> {
        let state = Box::new(State::new());
        Executor {
            // state: Box::into_raw(state),
            state,
            _marker: PhantomData,
        }
    }

    /// Returns `true` if there are no unfinished tasks.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// assert!(ex.is_empty());
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(!ex.is_empty());
    ///
    /// assert!(ex.try_tick());
    /// assert!(ex.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.state.active.lock().unwrap().is_empty()
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    pub fn spawn<T: Send + 'a>(
        &mut self,
        future: impl Future<Output = T> + 'a + Send + Sync,
    ) -> Task<T> {
        // Remove the task from the set of active tasks when the future finishes.
        let mut active = self.state.active.lock().unwrap();
        let entry = active.vacant_entry();
        let index = entry.key();

        let state = &self.state;

        let future = async move {
            let _guard = CallOnDrop(move || drop(state.active.lock().unwrap().try_remove(index)));
            future.await
        };

        // high order function that creates that sends runnable to the executor
        fn schedule(queue: &ConcurrentQueue<Runnable>) -> impl Fn(Runnable) + '_ + Send + Sync {
            move |runnable| queue.push(runnable).unwrap()
        }

        // Create the task and register it in the set of active tasks.
        //
        // SAFETY:
        //
        // Nothing here outlives the executor and the tasks dont need to be `Send` + `Sync` beacuse
        // this is not. This future must be dropped on this thread as it has no way to change
        // theads.
        let (runnable, task) = unsafe {
            Builder::new()
                .propagate_panic(true)
                .spawn_unchecked(|()| future, schedule(&self.state.queue))
        };
        entry.insert(runnable.waker());

        runnable.schedule();
        task
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use ex::Executor;
    ///
    /// let ex = Executor::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```
    pub fn try_tick(&mut self) -> bool {
        match self.state.queue.pop() {
            Err(_) => false,
            Ok(runnable) => {
                // Notify another ticker now to pick up where this ticker left off, just in case
                // running the task takes a long time.
                // self.state.notify();

                // Run the task.
                runnable.run();
                true
            }
        }
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use ex::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&mut self, future: impl Future<Output = T>) -> T {
        // A future that runs tasks forever.
        let run_forever = async {
            loop {
                for _ in 0..200 {
                    if !self.try_tick() {
                        break;
                    }
                }
                futures::yield_now().await;
            }
        };

        // Run `future` and `run_forever` concurrently until `future` completes.
        future.or(run_forever).await
    }

    pub fn exec(mut self) {
        while self.try_tick() {}
    }
}

impl Drop for Executor<'_> {
    fn drop(&mut self) {
        for w in self.state.active.get_mut().unwrap().drain() {
            w.wake();
        }

        while self.state.queue.pop().is_ok() {}
    }
}

impl<'a> Default for Executor<'a> {
    fn default() -> Executor<'a> {
        Executor::new()
    }
}

/// The state of a executor.
struct State {
    /// The global queue.
    queue: ConcurrentQueue<Runnable>,

    /// Currently active tasks.
    active: Mutex<Slab<Waker>>,
}

impl State {
    /// Creates state for a new executor.
    fn new() -> State {
        State {
            queue: ConcurrentQueue::unbounded(),
            active: Mutex::new(Slab::new()),
        }
    }
}

/// Runs a closure when dropped.
struct CallOnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
