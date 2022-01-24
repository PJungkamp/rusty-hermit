/// An executor, which is run when idling on network I/O.
use crate::net::nic;
use async_task::{Runnable, Task};
use concurrent_queue::ConcurrentQueue; use futures_lite::pin;
use smoltcp::time::{Duration, Instant};
use std::sync::atomic::Ordering;
use std::sync::{Mutex,MutexGuard};
use std::{
	future::Future,
	sync::{atomic::AtomicBool, Arc},
	task::{Context, Poll, Wake},
};

use hermit_abi::io;

/// A thread handle type
type Tid = u32;

extern "C" {
	fn sys_getpid() -> Tid;
	fn sys_yield();
	fn sys_wakeup_task(tid: Tid);
	fn sys_set_network_polling_mode(polling: bool);
	fn sys_block_current_task_with_timeout(timeout: u64);
	fn sys_block_current_task();
    fn sys_irq_disable() -> bool;
    fn sys_irq_enable();
}

pub(crate) struct PollingGuard { 
    executor: Option<MutexGuard<'static,Executor>> 
}

impl PollingGuard {
	pub fn new() -> Self {
		unsafe {
			sys_set_network_polling_mode(true);
		}
		Self {
            executor: Some(EXECUTOR.lock().unwrap())
        }
	}

    pub fn executor(&mut self) -> &mut Executor {
        self.executor.as_mut().unwrap()
    }

    pub fn yield_now(&mut self) {
        drop(self.executor.take());
		unsafe {
			sys_set_network_polling_mode(false);
            sys_yield();
            self.executor = Some(EXECUTOR.lock().unwrap());
            self.executor().run();
			sys_set_network_polling_mode(true);
		}
    }
}

impl Drop for PollingGuard {
	fn drop(&mut self) {
		unsafe { 
            let irq = sys_irq_disable();
            sys_set_network_polling_mode(false);
            self.executor().run();
            if irq { sys_irq_enable() }
        };
	}
}

pub(crate) fn run_nic_thread() -> Option<Duration> {
    EXECUTOR.lock().unwrap().run()
}

pub(crate) fn run_executor() -> Option<Duration> {
    let mut polling = PollingGuard::new();
    polling.executor().run()
}

lazy_static! {
	pub static ref QUEUE: ConcurrentQueue<Runnable> = ConcurrentQueue::unbounded();
    pub static ref EXECUTOR: Mutex<Executor> = Mutex::new(Executor);
}

pub struct Executor;

impl Executor {
    fn run(&mut self) -> Option<Duration> {
        let now = Instant::now();
        // make sure all futures register their waker with smoltcp before polling
        execute_all();
        let mut guard = Some(nic::lock());
        loop {
            guard.take().unwrap().with(|nic| nic.poll(now));
            // handle all futures woken by smoltcp
            execute_all();
            // if a future modified the socket_set repeat
            guard = Some(nic::lock());
            match guard.as_mut().unwrap().with(|nic| nic.was_woken()) {
                true => continue,
                false => break,
            }
        }
        let ret = guard.take().unwrap().with(|nic| nic.poll_delay(now));
        execute_all();
        ret 
    }
}

pub(crate) fn execute_all() {
	while let Ok(runnable) = QUEUE.pop() {
		runnable.run();
	}
}

struct ThreadNotify {
	/// The (single) executor thread.
	thread: Tid,
	/// A flag to ensure a wakeup is not "forgotten" before the next `block_current_task`
	unparked: AtomicBool,
	/// A flag to show that a wakeup occured
	woken: AtomicBool,
}

impl ThreadNotify {
	pub fn new() -> Self {
		Self {
			thread: unsafe { sys_getpid() },
			unparked: AtomicBool::new(false),
			woken: AtomicBool::new(false),
		}
	}

	pub fn was_woken(&self) -> bool {
		self.woken.load(Ordering::SeqCst)
	}

	pub fn was_unparked(&self) -> bool {
		self.unparked.load(Ordering::SeqCst)
	}

	pub fn reset_unparked(&self) {
		self.unparked.store(false, Ordering::SeqCst)
	}

	pub fn reset(&self) {
		self.woken.store(false, Ordering::SeqCst);
		self.unparked.store(false, Ordering::SeqCst);
	}
}

impl Wake for ThreadNotify {
	fn wake(self: Arc<Self>) {
		self.wake_by_ref()
	}

	fn wake_by_ref(self: &Arc<Self>) {
		debug!("waking thread_notify of Thread {}", self.thread);
		self.woken.store(true, Ordering::Release);
		// Make sure the wakeup is remembered until the next `park()`.
		let unparked = self.unparked.swap(true, Ordering::Release);
		if !unparked {
			unsafe {
				sys_wakeup_task(self.thread);
			}
		}
	}
}

/// Spawns a future on the executor.
///
/// if a future has not registered a waker
/// and it's task is never polled, it will leak memory
#[must_use]
pub fn spawn<F, T>(future: F) -> Task<T>
where
	F: Future<Output = T> + Send + 'static,
	T: Send + 'static,
{
	let schedule = |runnable| QUEUE.push(runnable).unwrap();
	let (runnable, task) = async_task::spawn(future, schedule);
	runnable.schedule();
	task
}

thread_local! {
	static CURRENT_THREAD_NOTIFY: Arc<ThreadNotify> = Arc::new(ThreadNotify::new());
}

/// Blocks the current thread on `f`, running the executor when idling.
pub fn block_on<F, T>(future: F, timeout: Option<Duration>) -> io::Result<T>
where
	F: Future<Output = T>,
{
	CURRENT_THREAD_NOTIFY.with(|thread_notify| {
		let start = Instant::now();

		let waker = thread_notify.clone().into();
		let mut cx = Context::from_waker(&waker);
		pin!(future);
		let mut polling = PollingGuard::new();
		loop {
			thread_notify.reset();
			if let Poll::Ready(t) = future.as_mut().poll(&mut cx) {
				trace!(
					"blocking future on thread {} is ready!",
					thread_notify.thread
				);
				return Ok(t);
			} else {
		        let pending_start = Instant::now();
				polling.executor().run();
				while !thread_notify.was_woken() {
                    let now = Instant::now();
					// check wheter to time out
					if let Some(duration) = timeout {
						if now >= start + duration {
							return Err(io::Error::new(
								io::ErrorKind::TimedOut,
								&"executor timed out",
							));
						}
					}

					// run executor before blocking
					trace!("checking network delay");
					let delay = polling.executor().run()
						.map(|d| d.total_millis());
					trace!("delay is {:?}", delay);

					// wait for the advised delay if it's greater than 100ms
					if now >= pending_start + Duration::from_millis(100)
                    && !thread_notify.was_woken() 
                    && (delay.is_none() || delay.unwrap() > 1000) {
						// deactivate the polling_mode when blocking
						if !thread_notify.was_unparked() {
						    debug!("blocking task");
							unsafe {
								match delay {
									Some(d) => sys_block_current_task_with_timeout(d),
									None => sys_block_current_task(),
								};
                            }
                            thread_notify.reset_unparked();
                            polling.yield_now();
                            polling.executor().run();
						}
					}
				}
				trace!("thread_notify was woken!");
			}
		}
	})
}
