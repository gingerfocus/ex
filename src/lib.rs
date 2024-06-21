#![warn(
    // missing_docs,
    missing_debug_implementations,
    rust_2018_idioms,
    clippy::undocumented_unsafe_blocks
)]
// #![no_std]

pub mod futures;

use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::task::{Poll, Waker};

use crate::futures::future::{self, Future, FutureExt};

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
///     });
/// }
/// println!("Hello world!");
/// drop(signal);
/// while ex.try_tick() {}
/// ```
pub struct Executor<'a> {
    /// The executor state.
    state: *mut State,

    /// Makes the `'a` lifetime invariant.
    _marker: PhantomData<std::cell::UnsafeCell<&'a ()>>,
}

impl UnwindSafe for Executor<'_> {}
impl RefUnwindSafe for Executor<'_> {}

impl fmt::Debug for Executor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();

        f.debug_struct("Executor")
            .field("active", &state.active.len())
            .field("global_tasks", &state.queue.len())
            .field("sleepers", &state.sleepers.count)
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
            state: Box::into_raw(state),
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
        // SAFETY: this doesnt change the state in anyway so it is safe to do
        self.state().active.is_empty()
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
    pub fn spawn<T: Send + 'a>(&mut self, future: impl Future<Output = T> + 'a) -> Task<T> {
        // SAFETY: `T` and the future are `Send`.
        self.spawn_inner(future, &mut self.state().active)
    }

    /// Spawns many tasks onto the executor.
    ///
    /// As opposed to the [`spawn`] method, this locks the executor's inner task lock once and
    /// spawns all of the tasks in one go. With large amounts of tasks this can improve
    /// contention.
    ///
    /// For very large numbers of tasks the lock is occasionally dropped and re-acquired to
    /// prevent runner thread starvation. It is assumed that the iterator provided does not
    /// block; blocking iterators can lock up the internal mutex and therefore the entire
    /// executor.
    ///
    /// ## Example
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::{stream, prelude::*};
    /// use std::future::ready;
    ///
    /// # futures_lite::future::block_on(async {
    /// let mut ex = Executor::new();
    ///
    /// let futures = [
    ///     ready(1),
    ///     ready(2),
    ///     ready(3)
    /// ];
    ///
    /// // Spawn all of the futures onto the executor at once.
    /// let mut tasks = vec![];
    /// ex.spawn_many(futures, &mut tasks);
    ///
    /// // Await all of them.
    /// let results = ex.run(async move {
    ///     stream::iter(tasks).then(|x| x).collect::<Vec<_>>().await
    /// }).await;
    /// assert_eq!(results, [1, 2, 3]);
    /// # });
    /// ```
    ///
    /// [`spawn`]: Executor::spawn
    pub fn spawn_many<T: Send + 'a, F: Future<Output = T> + Send + 'a>(
        &self,
        futures: impl IntoIterator<Item = F>,
        handles: &mut impl Extend<Task<F::Output>>,
    ) {
        let mut active = &mut self.state().active;

        // Convert the futures into tasks.
        let tasks = futures.into_iter().map(move |future| {
            // SAFETY: `T` and the future are `Send`.
            let task = self.spawn_inner(future, &mut active);

            task
        });

        // Push the tasks to the user's collection.
        handles.extend(tasks);
    }

    /// Spawn a future while holding the inner lock.
    ///
    /// # Safety
    ///
    /// If this is an `Executor`, `F` and `T` must be `Send`.
    fn spawn_inner<T: 'a>(
        &self,
        future: impl Future<Output = T> + 'a,
        active: &mut Slab<Waker>,
    ) -> Task<T> {
        // Remove the task from the set of active tasks when the future finishes.
        let entry = active.vacant_entry();
        let index = entry.key();
        let state = self.state();
        let future = async move {
            let _guard = CallOnDrop(move || drop(state.active.try_remove(index)));
            future.await
        };

        // Create the task and register it in the set of active tasks.
        //
        // SAFETY:
        //
        // Nothing here outlives the executor and the tasks dont need to be
        // `Send` + `Sync` beacuse this is not.
        let (runnable, task) = unsafe {
            Builder::new()
                .propagate_panic(true)
                .spawn_unchecked(|()| future, self.schedule())
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
        match self.state().queue.pop_front() {
            None => false,
            Some(runnable) => {
                // Notify another ticker now to pick up where this ticker left off, just in case
                // running the task takes a long time.
                self.state().notify();

                // Run the task.
                runnable.run();
                true
            }
        }
    }

    /// Runs a single task.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// future::block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        let state = self.state();
        let runnable = Ticker::new(state).runnable().await;
        runnable.run();
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        //let mut runner = Runner::new(self.state());
        //let mut rng = fastrand::Rng::with_seed(0xfa3c12e);

        // A future that runs tasks forever.
        let run_forever = async {
            loop {
                for _ in 0..200 {
                    self.tick().await;
                    //if let Some(runnable) = self.state().queue.pop_front() {
                    //    runnable.run();
                    //}
                }
                future::yield_now().await;
            }
        };

        // Run `future` and `run_forever` concurrently until `future` completes.
        future.or(run_forever).await
    }

    /// Returns a function that schedules a runnable task when it gets woken up.
    fn schedule(&self) -> impl Fn(Runnable) + 'static {
        let state = self.state;

        // TODO: If possible, push into the current local queue and notify the ticker.
        move |runnable| {
            let mut state = unsafe { state.read() };
            state.queue.push_back(runnable);
            state.notify();
        }
    }

    /// Returns a reference to the inner state.
    #[inline]
    fn state(&self) -> &mut State {
        // SAFETY: So long as an Executor lives, it's state pointer will always be valid
        unsafe { &mut *self.state }
    }
}

impl Drop for Executor<'_> {
    fn drop(&mut self) {
        let ptr = self.state;
        if ptr.is_null() {
            return;
        }

        // SAFETY: As ptr is not null, it was allocated via Box::new and converted
        // via Box::into_raw in state_ptr.
        let mut state = unsafe { Box::from_raw(ptr) };

        let active = &mut state.active;
        for w in active.drain() {
            w.wake();
        }

        while state.queue.pop_front().is_some() {}
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
    queue: VecDeque<Runnable>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: bool,

    /// A list of sleeping tickers.
    sleepers: Sleepers,

    /// Currently active tasks.
    active: Slab<Waker>,
}

impl State {
    /// Creates state for a new executor.
    fn new() -> State {
        State {
            queue: VecDeque::new(),
            notified: true,
            sleepers: Sleepers {
                count: 0,
                wakers: Vec::new(),
                free_ids: Vec::new(),
            },
            active: Slab::new(),
        }
    }

    /// Notifies a sleeping ticker.
    #[inline]
    fn notify(&mut self) {
        if !self.notified {
            self.notified = true;
            let waker = self.sleepers.notify();
            if let Some(w) = waker {
                w.wake();
            }
        }
    }
}

/// A list of sleeping tickers.
struct Sleepers {
    /// Number of sleeping tickers (both notified and unnotified).
    count: usize,

    /// IDs and wakers of sleeping unnotified tickers.
    ///
    /// A sleeping ticker is notified when its waker is missing from this list.
    wakers: Vec<(usize, Waker)>,

    /// Reclaimed IDs.
    free_ids: Vec<usize>,
}

impl Sleepers {
    /// Inserts a new sleeping ticker.
    fn insert(&mut self, waker: &Waker) -> usize {
        let id = match self.free_ids.pop() {
            Some(id) => id,
            None => self.count + 1,
        };
        self.count += 1;
        self.wakers.push((id, waker.clone()));
        id
    }

    /// Re-inserts a sleeping ticker's waker if it was notified.
    ///
    /// Returns `true` if the ticker was notified.
    fn update(&mut self, id: usize, waker: &Waker) -> bool {
        for item in &mut self.wakers {
            if item.0 == id {
                item.1.clone_from(waker);
                return false;
            }
        }

        self.wakers.push((id, waker.clone()));
        true
    }

    /// Removes a previously inserted sleeping ticker.
    ///
    /// Returns `true` if the ticker was notified.
    fn remove(&mut self, id: usize) -> bool {
        self.count -= 1;
        self.free_ids.push(id);

        for i in (0..self.wakers.len()).rev() {
            if self.wakers[i].0 == id {
                self.wakers.remove(i);
                return false;
            }
        }
        true
    }

    /// Returns `true` if a sleeping ticker is notified or no tickers are sleeping.
    fn is_notified(&self) -> bool {
        self.count == 0 || self.count > self.wakers.len()
    }

    /// Returns notification waker for a sleeping ticker.
    ///
    /// If a ticker was notified already or there are no tickers, `None` will be returned.
    fn notify(&mut self) -> Option<Waker> {
        if self.wakers.len() == self.count {
            self.wakers.pop().map(|item| item.1)
        } else {
            None
        }
    }
}

/// Runs task one by one.
struct Ticker<'a> {
    /// The executor state.
    state: *mut State,
    makerer: PhantomData<&'a mut State>,

    /// Set to a non-zero sleeper ID when in sleeping state.
    ///
    /// States a ticker can be in:
    /// 1) Woken.
    /// 2a) Sleeping and unnotified.
    /// 2b) Sleeping and notified.
    sleeping: usize,
}

impl Ticker<'_> {
    /// Creates a ticker.
    fn new(state: &State) -> Ticker<'_> {
        Ticker {
            state: state as *const State as *mut State,
            makerer: PhantomData,
            sleeping: 0,
        }
    }

    #[inline]
    fn state(&self) -> &'_ mut State {
        unsafe { &mut *self.state }
    }

    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&mut self, waker: &Waker) -> bool {
        match self.sleeping {
            // Move to sleeping state.
            0 => {
                self.sleeping = self.state().sleepers.insert(waker);
            }

            // Already sleeping, check if notified.
            id => {
                if !self.state().sleepers.update(id, waker) {
                    return false;
                }
            }
        }

        self.state().notified = self.state().sleepers.is_notified();

        true
    }

    /// Moves the ticker into woken state.
    fn wake(&mut self) {
        if self.sleeping != 0 {
            let sleepers = &mut self.state().sleepers;
            sleepers.remove(self.sleeping);

            self.state().notified = sleepers.is_notified();
        }
        self.sleeping = 0;
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&mut self) -> Runnable {
        let state = unsafe { &mut *self.state };
        future::poll_fn(|cx| loop {
            let task = state.queue.pop_front();
            match task {
                None => {
                    // Move to sleeping and unnotified state.
                    if !self.sleep(cx.waker()) {
                        // If already sleeping and unnotified, return.
                        return Poll::Pending;
                    }
                }
                Some(r) => {
                    // Wake up.
                    self.wake();

                    // Notify another ticker now to pick up where this ticker left off, just in
                    // case running the task takes a long time.
                    state.notify();

                    return Poll::Ready(r);
                }
            }
        })
        .await
    }
}

impl Drop for Ticker<'_> {
    fn drop(&mut self) {
        // If this ticker is in sleeping state, it must be removed from the sleepers list.
        if self.sleeping != 0 {
            let sleepers = &mut self.state().sleepers;
            let notified = sleepers.remove(self.sleeping);

            self.state().notified = sleepers.is_notified();

            // If this ticker was notified, then notify another ticker.
            if notified {
                self.state().notify();
            }
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
