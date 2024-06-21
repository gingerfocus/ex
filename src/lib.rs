#![warn(
    // missing_docs,
    missing_debug_implementations,
    rust_2018_idioms,
    clippy::undocumented_unsafe_blocks
)]
// #![no_std]

pub mod futures;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
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
    // state: *mut State,
    state: Box<State>,

    /// Makes the `'a` lifetime invariant.
    _marker: PhantomData<std::cell::UnsafeCell<&'a ()>>,
}

impl UnwindSafe for Executor<'_> {}
impl RefUnwindSafe for Executor<'_> {}

impl fmt::Debug for Executor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor")
            .field("active", &self.state.active.borrow().len())
            .field("global_tasks", &self.state.queue.borrow().len())
            .field("sleepers", &self.state.sleepers.borrow().count)
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
        // SAFETY: this doesnt change the state in anyway so it is safe to do
        self.state.active.borrow().is_empty()
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
        // Remove the task from the set of active tasks when the future finishes.
        let mut active = self.state.active.borrow_mut();
        let entry = active.vacant_entry();
        let index = entry.key();

        let state = &self.state;
        let future = async move {
            let _guard = CallOnDrop(move || drop(state.active.borrow_mut().try_remove(index)));
            future.await
        };

        // high order function that creates that sends runnable to the executor
        fn schedule(state: &State) -> impl Fn(Runnable) + '_ {
            move |runnable| {
                state.queue.borrow_mut().push_back(runnable);
                state.notify();
            }
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
                .spawn_unchecked(|()| future, schedule(&self.state))
        };
        // unsafe { async_task::spawn_unchecked(future, schedule(&self.state)) };
        // async_task::spawn_local(future, schedule)
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
        match self.state.queue.borrow_mut().pop_front() {
            None => false,
            Some(runnable) => {
                // Notify another ticker now to pick up where this ticker left off, just in case
                // running the task takes a long time.
                self.state.notify();

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
        let runnable = Ticker::new(&self.state).runnable().await;
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
    pub async fn run<T>(&mut self, future: impl Future<Output = T>) -> T {
        //let mut runner = Runner::new(self.state());
        //let mut rng = fastrand::Rng::with_seed(0xfa3c12e);

        // A future that runs tasks forever.
        let run_forever = async {
            loop {
                for _ in 0..200 {
                    self.tick().await;
                    //if let Some(runnable) = self.state.queue.borrow_mut().pop_front() {
                    //    runnable.run();
                    //}
                }
                future::yield_now().await;
            }
        };

        // Run `future` and `run_forever` concurrently until `future` completes.
        future.or(run_forever).await
    }
}

impl Drop for Executor<'_> {
    fn drop(&mut self) {
        for w in self.state.active.get_mut().drain() {
            w.wake();
        }

        while self.state.queue.get_mut().pop_front().is_some() {}
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
    queue: RefCell<VecDeque<Runnable>>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: AtomicBool,

    /// A list of sleeping tickers.
    sleepers: RefCell<Sleepers>,

    /// Currently active tasks.
    active: RefCell<Slab<Waker>>,
}

impl State {
    /// Creates state for a new executor.
    fn new() -> State {
        State {
            queue: RefCell::new(VecDeque::new()),
            notified: AtomicBool::new(true),
            sleepers: RefCell::new(Sleepers {
                count: 0,
                wakers: Vec::new(),
                free_ids: Vec::new(),
            }),
            active: RefCell::new(Slab::new()),
        }
    }

    /// Notifies a sleeping ticker.
    #[inline]
    fn notify(&self) {
        if self
            .notified
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let waker = self.sleepers.borrow_mut().notify();
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
    state: &'a State,

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
        Ticker { state, sleeping: 0 }
    }

    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&mut self, waker: &Waker) -> bool {
        match self.sleeping {
            // Move to sleeping state.
            0 => {
                self.sleeping = self.state.sleepers.borrow_mut().insert(waker);
            }

            // Already sleeping, check if notified.
            id => {
                if !self.state.sleepers.borrow_mut().update(id, waker) {
                    return false;
                }
            }
        }

        self.state.notified.store(
            self.state.sleepers.borrow_mut().is_notified(),
            Ordering::Relaxed,
        );

        true
    }

    /// Moves the ticker into woken state.
    fn wake(&mut self) {
        if self.sleeping != 0 {
            let sleepers = &self.state.sleepers;
            sleepers.borrow_mut().remove(self.sleeping);

            self.state
                .notified
                .store(sleepers.borrow().is_notified(), Ordering::Relaxed);
        }
        self.sleeping = 0;
    }

    /// Waits for the next runnable task to run.
    async fn runnable(&mut self) -> Runnable {
        future::poll_fn(|cx| loop {
            let task = self.state.queue.borrow_mut().pop_front();
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
                    self.state.notify();

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
            let sleepers = &self.state.sleepers;
            let notified = sleepers.borrow_mut().remove(self.sleeping);

            self.state
                .notified
                .store(sleepers.borrow().is_notified(), Ordering::Relaxed);

            // If this ticker was notified, then notify another ticker.
            if notified {
                self.state.notify();
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
