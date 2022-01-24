use crate::net::poll;
use crate::net::socket::AsyncSocket;
use crate::net::waker::WakerRegistration;
use futures_lite::future;
use hermit_abi::io;
use hermit_abi::net::Socket;
use smoltcp::time::Duration;
use std::sync::{Mutex, MutexGuard};
use std::task::{Poll, Waker};

lazy_static! {
	pub(crate) static ref SOCKETS: Mutex<SocketMap> = Mutex::new(SocketMap::new());
}

pub(crate) fn lock() -> MutexGuard<'static, SocketMap> {
	SOCKETS.lock().expect("SocketMap mutex poisoned")
}

/// this struct provides conversion from the exposed abi::Socket
/// to the internal entry
#[derive(Debug)]
pub(crate) struct SocketMap {
	sockets: Vec<Option<SocketEntry>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
	pub non_blocking: bool,
	pub timeout: Option<Duration>,
}

/// the socket_entry manages options related to calling behaviour (like non_blocking),
/// to access settings for lower levels or query things about the socket
/// the async_socket within is used
#[derive(Debug)]
pub(crate) struct SocketEntry {
	/// the async socket to perform operations on
	pub async_socket: AsyncSocket,
	/// options for calling behaviour of a socket
	pub options: Options,
	// whether this entry is closing
	pub closing: bool,
	// this task registers it's waker on the socket
	// and on wake drains and wakes all recv_wakers
	// it must be woken when the underlying socket changes
	recv_task_waker: WakerRegistration,
	// this task registers it's waker on the socket
	// and on wake drains and wakes all send_wakers
	// it must be woken when the underlying socket changes
	send_task_waker: WakerRegistration,
	// all registered recv wakers
	recv_wakers: Vec<Waker>,
	// all registered send wakers
	send_wakers: Vec<Waker>,
}

impl SocketEntry {
	/// split the entry into an async_socket and it's options
	pub(crate) fn split_ref(&self) -> (&AsyncSocket, &Options) {
		(&self.async_socket, &self.options)
	}

	/// split the entry into an async_socket and it's options
	pub(crate) fn split_mut(&mut self) -> (&mut AsyncSocket, &mut Options) {
		(&mut self.async_socket, &mut self.options)
	}

	/// register a send waker
	///
	/// this is safe to call for different wakers
	pub(crate) fn register_send_waker(&mut self, waker: &Waker) {
		debug!("register send waker");
		self.send_wakers.push(waker.clone());
	}

	/// register a recv waker
	///
	/// this is safe to call for different wakers
	pub(crate) fn register_recv_waker(&mut self, waker: &Waker) {
		debug!("register recv waker");
		self.recv_wakers.push(waker.clone());
	}

	pub(crate) fn insert_async_socket(&mut self, async_socket: AsyncSocket) {
		self.async_socket = async_socket;
		self.wake_tasks();
	}

	/// wake the associated tasks to notify them of changes to this socket
	pub(crate) fn wake_tasks(&mut self) {
		self.send_task_waker.wake();
		self.recv_task_waker.wake();
	}

	/// wake all registered recv wakers
	fn wake_recv(&mut self) {
		debug!("waking {} recv wakers", self.recv_wakers.len());
		for waker in self.recv_wakers.drain(..) {
			waker.wake();
		}
	}

	/// wake all registered recv wakers
	fn wake_send(&mut self) {
		debug!("waking {} send wakers", self.send_wakers.len());
		for waker in self.send_wakers.drain(..) {
			waker.wake();
		}
	}
}

impl SocketMap {
	/// create a new SocketMap
	fn new() -> Self {
		Self {
			sockets: Vec::with_capacity(256),
		}
	}

	/// get a vacant entry or allocate one if none exist
	fn next_free_entry(&mut self) -> Socket {
		// find a vacant slot or create a new one
		let id = self
			.sockets
			.iter()
			.enumerate()
			.find_map(|(id, entry)| entry.is_none().then(|| id))
			.unwrap_or_else(|| {
				self.sockets.push(None);
				self.sockets.len() - 1
			});
		Socket { id }
	}

	/// register an exclusive waker for the relevant socket and notification wakers on other
	/// relevant sockets
	pub fn register_exclusive_waker(
		&mut self,
		socket: Socket,
		waker: &Waker,
		wake_on: poll::WakeOn,
	) {
		self.get_mut(socket)
			.and_then(|entry| {
				entry
					.async_socket
					.as_socket_mut()
					.map(|socket| match wake_on {
						poll::WakeOn::Send => socket.register_exclusive_send_waker(waker),
						poll::WakeOn::Recv => socket.register_exclusive_recv_waker(waker),
						poll::WakeOn::SendRecv => {
							if let Some((mut s1, mut r1)) =
								socket.register_exclusive_send_waker(waker)
							{
								if let Some((s2, r2)) = socket.register_exclusive_recv_waker(waker)
								{
									s1.extend_from_slice(&s2);
									r1.extend_from_slice(&r2);
									s1.dedup();
									r1.dedup();
								}
								Some((s1, r1))
							} else {
								socket.register_exclusive_recv_waker(waker)
							}
						}
					})
			})
			.ok()
			.flatten()
			.map(|(s, r)| {
				for send in s {
					trace!(
						"register transient send waker for {:?} on {:?}",
						socket,
						send
					);
					self.get_mut(send)
						.map(|entry| entry.register_send_waker(waker))
						.unwrap_or_else(|_| warn!("registering send waker on unknown socket"));
				}
				for recv in r {
					trace!(
						"register transient recv waker for {:?} on {:?}",
						socket,
						recv
					);
					self.get_mut(recv)
						.map(|entry| entry.register_recv_waker(waker))
						.unwrap_or_else(|_| warn!("registering send waker on unknown socket"));
				}
			})
			.unwrap_or(());
	}

	/// close a socket by removing it's entry
	/// cancelling the wake futures
	pub(crate) fn take(&mut self, socket: Socket) -> io::Result<SocketEntry> {
		self.sockets
			.get_mut(socket.id)
			.and_then(Option::take)
			.map(|mut entry| {
				entry.wake_tasks();
				entry
			})
			.ok_or(io::Error::new(io::ErrorKind::NotFound, &"unknown socket"))
	}

	pub(crate) fn get(&self, socket: Socket) -> io::Result<&SocketEntry> {
		self.sockets
			.get(socket.id)
			.and_then(Option::as_ref)
			.ok_or(io::Error::new(io::ErrorKind::NotFound, &"unknown socket"))
	}

	pub(crate) fn get_mut(&mut self, socket: Socket) -> io::Result<&mut SocketEntry> {
		self.sockets
			.get_mut(socket.id)
			.and_then(Option::as_mut)
			.ok_or(io::Error::new(io::ErrorKind::NotFound, &"unknown socket"))
	}

	/// insert an async socket and update daemon futures
	pub(crate) fn bind_socket(
		&mut self,
		socket: Socket,
		async_socket: AsyncSocket,
	) -> io::Result<()> {
		let entry = self.get_mut(socket)?;
		entry.insert_async_socket(async_socket);
		entry.wake_tasks();
		Ok(())
	}

	pub(crate) fn new_socket(
		&mut self,
		options: Options,
	) -> (
		Socket,
		impl future::Future<Output = ()>,
		impl future::Future<Output = ()>,
	) {
		// get a free entry
		let socket = self.next_free_entry();
		// spawn the waker futures for this entry
		let send_future = future::poll_fn(move |cx| {
			let mut sockets = lock();
			if let Ok(entry) = sockets.get_mut(socket) {
				trace!("waking send for {:?}", socket);
				entry.send_task_waker.register(cx.waker());
				entry.wake_send();
				if entry.closing {
					Poll::Ready(())
				} else {
				    sockets.register_exclusive_waker(socket, cx.waker(), poll::WakeOn::Send);
				    Poll::Pending
                }
			} else {
				Poll::Ready(())
			}
		});
		let recv_future = future::poll_fn(move |cx| {
			let mut sockets = lock();
			if let Ok(entry) = sockets.get_mut(socket) {
				if entry.closing {
					return Poll::Ready(());
				}
				trace!("waking recv for {:?}", socket);
				entry.recv_task_waker.register(cx.waker());
				entry.wake_recv();
				if entry.closing {
					Poll::Ready(())
				} else {
                    sockets.register_exclusive_waker(socket, cx.waker(), poll::WakeOn::Recv);
                    Poll::Pending
                }
			} else {
				Poll::Ready(())
			}
		});
		// insert the socket
		let _ = self.sockets[socket.id].insert(SocketEntry {
			async_socket: AsyncSocket::Unbound,
			options,
			closing: false,
			send_task_waker: WakerRegistration::new(),
			recv_task_waker: WakerRegistration::new(),
			send_wakers: Vec::with_capacity(4),
			recv_wakers: Vec::with_capacity(4),
		});
		(socket, send_future, recv_future)
	}
}
