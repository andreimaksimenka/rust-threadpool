// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Abstraction of a thread pool for basic parallelism.

use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{Builder, panicking};

trait FnBox {
    fn call_box(self: Box<Self>);
}

impl<F: FnOnce()> FnBox for F {
    fn call_box(self: Box<F>) {
        (*self)()
    }
}

type Thunk<'a> = Box<FnBox + Send + 'a>;

struct Sentinel<'a> {
    name: Option<String>,
    jobs: &'a Arc<Mutex<Receiver<Thunk<'static>>>>,
    thread_counter: &'a Arc<AtomicUsize>,
    thread_count_spawned: &'a Arc<AtomicUsize>,
    thread_count_min: &'a Arc<AtomicUsize>,
    thread_count_max: &'a Arc<AtomicUsize>,
    thread_count_panic: &'a Arc<AtomicUsize>,
    active: bool,
}

impl<'a> Sentinel<'a> {
    fn new(name: Option<String>,
           jobs: &'a Arc<Mutex<Receiver<Thunk<'static>>>>,
           thread_counter: &'a Arc<AtomicUsize>,
           thread_count_spawned: &'a Arc<AtomicUsize>,
           thread_count_min: &'a Arc<AtomicUsize>,
           thread_count_max: &'a Arc<AtomicUsize>,
           thread_count_panic: &'a Arc<AtomicUsize>)
           -> Sentinel<'a> {
        Sentinel {
            name: name,
            jobs: jobs,
            thread_counter: thread_counter,
            thread_count_spawned: thread_count_spawned,
            thread_count_min: thread_count_min,
            thread_count_max: thread_count_max,
            thread_count_panic: thread_count_panic,
            active: true,
        }
    }

    // Cancel and destroy this sentinel.
    fn cancel(mut self) {
        self.active = false;
    }
}

impl<'a> Drop for Sentinel<'a> {
    fn drop(&mut self) {
        if self.active {
            if panicking() {
                self.thread_count_panic.fetch_add(1, Ordering::SeqCst);
            }
            if self.thread_counter.fetch_sub(1, Ordering::SeqCst) == 1 &&
               self.thread_count_spawned.load(Ordering::Acquire) <=
               self.thread_count_min.load(Ordering::Relaxed) {
                self.thread_count_spawned.fetch_add(1, Ordering::SeqCst);
                spawn_in_pool(self.name.clone(),
                              self.jobs.clone(),
                              self.thread_counter.clone(),
                              self.thread_count_spawned.clone(),
                              self.thread_count_min.clone(),
                              self.thread_count_max.clone(),
                              self.thread_count_panic.clone());
            }
        }
    }
}

/// A thread pool used to execute functions in parallel.
///
/// Spawns `n` worker threads and replenishes the pool if any worker threads
/// panic.
///
/// # Examples
///
/// ## Syncronized with a channel
///
/// Every thread sends one message over the channel, which then is collected with the `take()`.
///
/// ```
/// use threadpool::ThreadPool;
/// use std::sync::mpsc::channel;
///
/// let n_workers = 4;
/// let n_jobs = 8;
/// let pool = ThreadPool::new(n_workers);
///
/// let (tx, rx) = channel();
/// for i in 0..n_jobs {
///     let tx = tx.clone();
///     pool.execute(move|| {
///         tx.send(i).unwrap();
///     });
/// }
///
/// assert_eq!(rx.iter().take(n_jobs).fold(0, |a, b| a + b), 28);
/// ```
///
/// ## Syncronized with a barrier
///
/// Keep in mind, if you put more jobs in the pool than you have workers,
/// you will end up with a [deadlock](https://en.wikipedia.org/wiki/Deadlock) which is
/// [not considered unsafe](http://doc.rust-lang.org/reference.html#behavior-not-considered-unsafe).
///
/// ```
/// use threadpool::ThreadPool;
/// use std::sync::{Arc, Barrier};
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// // create at least as many workers as jobs or you will deadlock yourself
/// let n_workers = 42;
/// let n_jobs = 23;
/// let pool = ThreadPool::new(n_workers);
/// let an_atomic = Arc::new(AtomicUsize::new(0));
///
/// // create a barrier that wait all jobs plus the starter thread
/// let barrier = Arc::new(Barrier::new(n_jobs + 1));
/// for i in 0..n_jobs {
///   let barrier = barrier.clone();
///   let an_atomic = an_atomic.clone();
///
///   pool.execute(move|| {
///     // do the heavy work
///     an_atomic.fetch_add(1, Ordering::Relaxed);
///
///     // then wait for the other threads
///     barrier.wait();
///   });
/// }
///
/// // wait for the threads to finish the work
/// barrier.wait();
/// assert_eq!(an_atomic.load(Ordering::SeqCst), 23);
/// ```
#[derive(Clone)]
pub struct ThreadPool {
    // How the threadpool communicates with subthreads.
    //
    // This is the only such Sender, so when it is dropped all subthreads will
    // quit.
    name: Option<String>,
    jobs: Sender<Thunk<'static>>,
    job_receiver: Arc<Mutex<Receiver<Thunk<'static>>>>,
    active_count: Arc<AtomicUsize>,
    spawned_count: Arc<AtomicUsize>,
    min_count: Arc<AtomicUsize>,
    max_count: Arc<AtomicUsize>,
    panic_count: Arc<AtomicUsize>,
}

impl ThreadPool {
    /// Spawns a new thread pool with `num_threads` threads.
    ///
    /// # Panics
    ///
    /// This function will panic if `num_threads` is 0.
    pub fn new(num_threads: usize) -> ThreadPool {
        ThreadPool::new_pool(None, num_threads, num_threads)
    }

    /// Spawns a new dynamic thread pool with `num_threads` maximum threads and
    /// `num_initial_threads` initial threads. The thread pool will adjust number
    /// of running OS threads depending on thread pool utilization. The thread
    /// pool will try to keep number of running threads at most `num_threads` and
    /// at least `num_initial_threads` but those are soft boundaries and may be
    /// temporarily exceeded for efficiency reasons. The tread pool will drop OS
    /// threads if there are no active jobs left.
    ///
    /// # Panics
    ///
    /// This function will panic if `num_threads` or `num_initial_threads` is 0,
    /// or if `num_initial_threads` is greater than `num_threads`.
    pub fn new_dynamic(num_threads: usize, num_initial_threads: usize) -> ThreadPool {
        ThreadPool::new_pool(None, num_threads, num_initial_threads)
    }

    /// Spawns a new thread pool with `num_threads` threads. Each thread will have the
    /// [name][thread name] `name`.
    ///
    /// # Panics
    ///
    /// This function will panic if `num_threads` is 0.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::mpsc::sync_channel;
    /// use std::thread;
    /// use threadpool::ThreadPool;
    ///
    /// let (tx, rx) = sync_channel(0);
    /// let mut pool = ThreadPool::new_with_name("worker".into(), 2);
    /// for _ in 0..2 {
    ///     let tx = tx.clone();
    ///     pool.execute(move || {
    ///         let name = thread::current().name().unwrap().to_owned();
    ///         tx.send(name).unwrap();
    ///     });
    /// }
    ///
    /// for thread_name in rx.iter().take(2) {
    ///     assert_eq!("worker", thread_name);
    /// }
    /// ```
    ///
    /// [thread name]: https://doc.rust-lang.org/std/thread/struct.Thread.html#method.name
    pub fn new_with_name(name: String, num_threads: usize) -> ThreadPool {
        ThreadPool::new_pool(Some(name), num_threads, num_threads)
    }

    /// Spawns a new dynamic thread pool with `num_threads` maximum threads and
    /// `num_initial_threads` initial threads. Each thread will have the [name][thread name] `name`.
    ///
    /// # Panics
    ///
    /// This function will panic if `num_threads` or `num_initial_threads` is 0,
    /// or if `num_initial_threads` is greater than `num_threads`.
    pub fn new_with_name_dynamic(name: String,
                                 num_threads: usize,
                                 num_initial_threads: usize)
                                 -> ThreadPool {
        ThreadPool::new_pool(Some(name), num_threads, num_initial_threads)
    }

    #[inline]
    fn new_pool(name: Option<String>,
                num_threads: usize,
                num_initial_threads: usize)
                -> ThreadPool {
        assert!(num_threads >= 1);
        assert!(num_initial_threads >= 1);
        assert!(num_initial_threads <= num_threads);

        let (tx, rx) = channel::<Thunk<'static>>();
        let rx = Arc::new(Mutex::new(rx));
        let active_count = Arc::new(AtomicUsize::new(0));
        let spawned_count = Arc::new(AtomicUsize::new(0));
        let min_count = Arc::new(AtomicUsize::new(num_initial_threads));
        let max_count = Arc::new(AtomicUsize::new(num_threads));
        let panic_count = Arc::new(AtomicUsize::new(0));

        // Threadpool threads
        for _ in 0..num_initial_threads {
            spawned_count.fetch_add(1, Ordering::SeqCst);
            spawn_in_pool(name.clone(),
                          rx.clone(),
                          active_count.clone(),
                          spawned_count.clone(),
                          min_count.clone(),
                          max_count.clone(),
                          panic_count.clone());
        }

        ThreadPool {
            name: name,
            jobs: tx,
            job_receiver: rx.clone(),
            active_count: active_count,
            spawned_count: spawned_count,
            min_count: min_count,
            max_count: max_count,
            panic_count: panic_count,
        }
    }

    /// Executes the function `job` on a thread in the pool.
    pub fn execute<F>(&self, job: F)
        where F: FnOnce() + Send + 'static
    {
        // Spawn a new thread if the pool is dynamically managed and the number
        // of active threads is smaller than the maximum allowed.
        if self.spawned_count.load(Ordering::Acquire) < self.max_count.load(Ordering::Relaxed) {
            self.spawned_count.fetch_add(1, Ordering::SeqCst);
            spawn_in_pool(self.name.clone(),
                          self.job_receiver.clone(),
                          self.active_count.clone(),
                          self.spawned_count.clone(),
                          self.min_count.clone(),
                          self.max_count.clone(),
                          self.panic_count.clone());
        }
        self.jobs.send(Box::new(move || job())).unwrap();
    }

    /// Returns the number of currently active threads.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Returns the number of spawned threads.
    pub fn spawned_count(&self) -> usize {
        self.spawned_count.load(Ordering::Relaxed)
    }

    /// Returns the minimum number of created threads.
    pub fn min_count(&self) -> usize {
        self.min_count.load(Ordering::Relaxed)
    }

    /// Returns the maximum number of created threads.
    pub fn max_count(&self) -> usize {
        self.max_count.load(Ordering::Relaxed)
    }

    /// Returns the number of panicked threads over the lifetime of the pool.
    pub fn panic_count(&self) -> usize {
        self.panic_count.load(Ordering::Relaxed)
    }

    /// **Deprecated: Use `ThreadPool::set_num_threads`**
    // #[deprecated(since = "1.3.0", note = "use ThreadPool::set_num_threads")]
    // TODO: #[deprecated] isn't stable yet.
    pub fn set_threads(&mut self, num_threads: usize) {
        self.set_num_threads(num_threads)
    }

    /// Sets the number of worker-threads to use as `num_threads`.
    /// Can be used to change the threadpool size during runtime.
    /// Will not abort already running or waiting threads.
    pub fn set_num_threads(&mut self, num_threads: usize) {
        assert!(num_threads >= 1);
        let current_max = (*self.max_count).swap(num_threads, Ordering::Release);
        if num_threads > current_max {
            // Spawn new threads
            for _ in 0..(num_threads - current_max) {
                self.spawned_count.fetch_add(1, Ordering::SeqCst);
                spawn_in_pool(self.name.clone(),
                              self.job_receiver.clone(),
                              self.active_count.clone(),
                              self.spawned_count.clone(),
                              self.min_count.clone(),
                              self.max_count.clone(),
                              self.panic_count.clone());
            }
        }
    }
}

fn spawn_in_pool(name: Option<String>,
                 jobs: Arc<Mutex<Receiver<Thunk<'static>>>>,
                 thread_counter: Arc<AtomicUsize>,
                 thread_count_spawned: Arc<AtomicUsize>,
                 thread_count_min: Arc<AtomicUsize>,
                 thread_count_max: Arc<AtomicUsize>,
                 thread_count_panic: Arc<AtomicUsize>) {
    let mut builder = Builder::new();
    if let Some(ref name) = name {
        builder = builder.name(name.clone());
    }
    builder.spawn(move || {

            // Will spawn a new thread on panic unless it is cancelled.
            let sentinel = Sentinel::new(name,
                                         &jobs,
                                         &thread_counter,
                                         &thread_count_spawned,
                                         &thread_count_min,
                                         &thread_count_max,
                                         &thread_count_panic);

            loop {
                // Shutdown this thread if the pool has become smaller
                let thread_counter_val = thread_counter.load(Ordering::Acquire);
                let thread_count_min_val = thread_count_min.load(Ordering::Relaxed);
                let thread_count_max_val = thread_count_max.load(Ordering::Relaxed);
                if thread_counter_val < thread_count_max_val {
                    let message = {
                        // Only lock jobs for the time it takes
                        // to get a job, not run it.
                        let lock = jobs.lock().unwrap();
                        lock.recv()
                    };

                    match message {
                        Ok(job) => {
                            // Do not allow IR around the job execution
                            thread_counter.fetch_add(1, Ordering::SeqCst);
                            job.call_box();
                            thread_counter.fetch_sub(1, Ordering::SeqCst);
                            // Shutdown this thread if there are no active jobs and number of
                            // spawned threads more than the minimum.
                            if thread_count_min_val != thread_count_max_val &&
                               thread_counter.load(Ordering::Acquire) == 0 &&
                               thread_count_spawned.load(Ordering::Acquire) > thread_count_min_val {
                                break;
                            }
                        }

                        // The ThreadPool was dropped.
                        Err(..) => break,
                    }
                } else {
                    break;
                }
            }

            thread_count_spawned.fetch_sub(1, Ordering::SeqCst);
            sentinel.cancel();
        })
        .unwrap();
}

#[cfg(test)]
mod test {
    use super::ThreadPool;
    use std::sync::mpsc::{sync_channel, channel};
    use std::sync::{Arc, Barrier};
    use std::thread::{self, sleep};
    use std::time::Duration;

    const TEST_TASKS: usize = 4;

    #[test]
    fn test_set_num_threads_increasing() {
        let new_thread_amount = TEST_TASKS + 8;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                loop {
                    sleep(Duration::from_secs(10))
                }
            });
        }
        pool.set_num_threads(new_thread_amount);
        for _ in 0..(new_thread_amount - TEST_TASKS) {
            pool.execute(move || {
                loop {
                    sleep(Duration::from_secs(10))
                }
            });
        }
        sleep(Duration::from_secs(1));
        assert_eq!(pool.active_count(), new_thread_amount);
    }

    #[test]
    fn test_set_num_threads_decreasing() {
        let new_thread_amount = 2;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                1 + 1;
            });
        }
        pool.set_num_threads(new_thread_amount);
        for _ in 0..new_thread_amount {
            pool.execute(move || {
                loop {
                    sleep(Duration::from_secs(10))
                }
            });
        }
        sleep(Duration::from_secs(1));
        assert_eq!(pool.active_count(), new_thread_amount);
    }

    #[test]
    fn test_active_count() {
        let pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                loop {
                    sleep(Duration::from_secs(10))
                }
            });
        }
        sleep(Duration::from_secs(1));
        let active_count = pool.active_count();
        assert_eq!(active_count, TEST_TASKS);
        let initialized_count = pool.max_count();
        assert_eq!(initialized_count, TEST_TASKS);
    }

    #[test]
    fn test_works() {
        let pool = ThreadPool::new(TEST_TASKS);

        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move || {
                tx.send(1).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    #[should_panic]
    fn test_zero_tasks_panic() {
        ThreadPool::new(0);
    }

    #[test]
    fn test_recovery_from_subtask_panic() {
        let pool = ThreadPool::new(TEST_TASKS);

        // Panic all the existing threads.
        for _ in 0..TEST_TASKS {
            pool.execute(move || -> () { panic!() });
        }
        sleep(Duration::from_secs(1));

        assert_eq!(pool.panic_count(), TEST_TASKS);

        // Ensure new threads were spawned to compensate.
        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move || {
                tx.send(1).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    fn test_should_not_panic_on_drop_if_subtasks_panic_after_drop() {

        let pool = ThreadPool::new(TEST_TASKS);
        let waiter = Arc::new(Barrier::new(TEST_TASKS + 1));

        // Panic all the existing threads in a bit.
        for _ in 0..TEST_TASKS {
            let waiter = waiter.clone();
            pool.execute(move || {
                waiter.wait();
                panic!("Ignore this panic, it should!");
            });
        }

        drop(pool);

        // Kick off the failure.
        waiter.wait();
    }

    #[test]
    fn test_massive_task_creation() {
        let test_tasks = 4_200_000;

        let pool = ThreadPool::new(TEST_TASKS);
        let b0 = Arc::new(Barrier::new(TEST_TASKS + 1));
        let b1 = Arc::new(Barrier::new(TEST_TASKS + 1));

        let (tx, rx) = channel();

        for i in 0..test_tasks {
            let tx = tx.clone();
            let (b0, b1) = (b0.clone(), b1.clone());

            pool.execute(move || {

                // Wait until the pool has been filled once.
                if i < TEST_TASKS {
                    b0.wait();
                    // wait so the pool can be measured
                    b1.wait();
                }

                tx.send(1).is_ok();
            });
        }

        b0.wait();
        assert_eq!(pool.active_count(), TEST_TASKS);
        b1.wait();

        assert_eq!(rx.iter().take(test_tasks).fold(0, |a, b| a + b), test_tasks);
        // `iter().take(test_tasks).fold` may be faster than the last thread finishing itself, so
        // values of 0 or 1 are ok.
        assert!(pool.active_count() <= 1);
    }

    #[test]
    fn test_shrink() {
        let test_tasks_begin = TEST_TASKS + 2;

        let mut pool = ThreadPool::new(test_tasks_begin);
        let b0 = Arc::new(Barrier::new(test_tasks_begin + 1));
        let b1 = Arc::new(Barrier::new(test_tasks_begin + 1));

        for _ in 0..test_tasks_begin {
            let (b0, b1) = (b0.clone(), b1.clone());
            pool.execute(move || {
                b0.wait();
                b1.wait();
            });
        }

        let b2 = Arc::new(Barrier::new(TEST_TASKS + 1));
        let b3 = Arc::new(Barrier::new(TEST_TASKS + 1));

        for _ in 0..TEST_TASKS {
            let (b2, b3) = (b2.clone(), b3.clone());
            pool.execute(move || {
                b2.wait();
                b3.wait();
            });
        }

        b0.wait();
        pool.set_num_threads(TEST_TASKS);

        assert_eq!(pool.active_count(), test_tasks_begin);
        b1.wait();

        b2.wait();
        assert_eq!(pool.active_count(), TEST_TASKS);
        b3.wait();
    }

    #[test]
    fn test_shrink_then_grow_dynamic() {
        let test_tasks_begin = TEST_TASKS + 2;

        let mut pool = ThreadPool::new_dynamic(test_tasks_begin, 1);
        assert_eq!(pool.spawned_count(), 1);

        let b0 = Arc::new(Barrier::new(test_tasks_begin + 1));
        let b1 = Arc::new(Barrier::new(test_tasks_begin + 1));

        for _ in 0..test_tasks_begin {
            let (b0, b1) = (b0.clone(), b1.clone());
            pool.execute(move || {
                b0.wait();
                b1.wait();
            });
        }
        assert_eq!(pool.spawned_count(), test_tasks_begin);

        let b2 = Arc::new(Barrier::new(TEST_TASKS + 1));
        let b3 = Arc::new(Barrier::new(TEST_TASKS + 1));

        for _ in 0..TEST_TASKS {
            let (b2, b3) = (b2.clone(), b3.clone());
            pool.execute(move || {
                b2.wait();
                b3.wait();
            });
        }

        b0.wait();
        pool.set_num_threads(TEST_TASKS);

        assert_eq!(pool.active_count(), test_tasks_begin);
        assert_eq!(pool.spawned_count(), test_tasks_begin);
        b1.wait();

        b2.wait();
        assert_eq!(pool.active_count(), TEST_TASKS);
        assert_eq!(pool.spawned_count(), TEST_TASKS);
        b3.wait();

        pool.set_num_threads(test_tasks_begin);
        let b4 = Arc::new(Barrier::new(test_tasks_begin + 1));
        let b5 = Arc::new(Barrier::new(test_tasks_begin + 1));

        for _ in 0..test_tasks_begin {
            let (b4, b5) = (b4.clone(), b5.clone());
            pool.execute(move || {
                b4.wait();
                b5.wait();
            });
        }
        b4.wait();
        assert_eq!(pool.spawned_count(), test_tasks_begin);
        b5.wait();
    }

    #[test]
    fn test_name() {
        let name = "test";
        let mut pool = ThreadPool::new_with_name(name.to_owned(), 2);
        let (tx, rx) = sync_channel(0);

        // initial thread should share the name "test"
        for _ in 0..2 {
            let tx = tx.clone();
            pool.execute(move || {
                let name = thread::current().name().unwrap().to_owned();
                tx.send(name).unwrap();
            });
        }

        // new spawn thread should share the name "test" too.
        pool.set_num_threads(3);
        let tx_clone = tx.clone();
        pool.execute(move || {
            let name = thread::current().name().unwrap().to_owned();
            tx_clone.send(name).unwrap();
            panic!();
        });

        // recover thread should share the name "test" too.
        pool.execute(move || {
            let name = thread::current().name().unwrap().to_owned();
            tx.send(name).unwrap();
        });

        for thread_name in rx.iter().take(4) {
            assert_eq!(name, thread_name);
        }
    }
}
