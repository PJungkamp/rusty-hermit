use crate::net::{
	nic,
    socket_map,
	poll::{Poll, PollSocketRaw, PollSocketsRaw, WakeOn},
	socket::HandleWrapper,
};
use hermit_abi::io;
use hermit_abi::net;
use smoltcp::socket::TcpSocket;
use smoltcp::wire::IpEndpoint;
use std::ops::Not;
use std::task::Waker;

#[derive(Debug, Clone)]
enum Inner {
	Handle(HandleWrapper),
	Backlog(Vec<HandleWrapper>),
}

impl Inner {
	fn as_handle(&self) -> io::Result<&HandleWrapper> {
		match self {
			Self::Handle(handle) => Ok(handle),
			Self::Backlog(_) => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"this socket is listening",
			)),
		}
	}

	fn as_backlog_ref(&self) -> io::Result<&Vec<HandleWrapper>> {
		match self {
			Self::Backlog(vec) => Ok(vec),
			Self::Handle(_) => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"this socket is not listening",
			)),
		}
	}

	fn as_backlog_mut(&mut self) -> io::Result<&mut Vec<HandleWrapper>> {
		match self {
			Self::Backlog(vec) => Ok(vec),
			Self::Handle(_) => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"this socket is not listening",
			)),
		}
	}
}

/// Default keep alive interval in milliseconds
const DEFAULT_KEEP_ALIVE_INTERVAL: u64 = 75000;

#[derive(Debug)]
pub(crate) struct AsyncTcpSocket {
	/// handle to the smoltcp socket in the nic
	inner: Inner,
	/// the bound local endpoint
	local: IpEndpoint,
	/// wheter write() is allowed
	///
	/// since shutdown() waits for remaining packets
	/// by the tcp statemachine writeable socket
	/// may not allow writing since closing is in progress
	allow_write: bool,
	/// wheter read() is allowed
	///
	/// since shutdown() can not actually influence the read
	/// behaviour of a socket (only the peer can close the connection)
	/// this option inhibits reads after shutdown has been called
	allow_read: bool,
}

impl super::Socket for AsyncTcpSocket {
	fn register_exclusive_send_waker(
		&mut self,
		waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		nic::lock().with(|nic| match self.inner {
			Inner::Handle(ref handle) => nic.with_mut(handle, |tcp_socket: &mut TcpSocket| {
				tcp_socket.register_send_waker(waker)
			}),
			Inner::Backlog(ref backlog) => {
				for handle in backlog.iter() {
					nic.with_mut(handle, |tcp_socket: &mut TcpSocket| {
						tcp_socket.register_send_waker(waker)
					})
				}
			}
		});
		None
	}

	fn register_exclusive_recv_waker(
		&mut self,
		waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		nic::lock().with(|nic| match self.inner {
			Inner::Handle(ref handle) => nic.with_mut(handle, |tcp_socket: &mut TcpSocket| {
				tcp_socket.register_recv_waker(waker)
			}),
			Inner::Backlog(ref backlog) => {
				for handle in backlog.iter() {
					nic.with_mut(handle, |tcp_socket: &mut TcpSocket| {
						tcp_socket.register_recv_waker(waker)
					})
				}
			}
		});
		None
	}

	fn get_event_flags(&self, _: &socket_map::SocketMap) -> net::event::EventFlags {
		use net::event::EventFlags;
		nic::lock().with(|nic| match self.inner {
			Inner::Handle(ref handle) => nic.with_ref(handle, |tcp: &TcpSocket| {
				let mut flags = EventFlags::NONE;
				if tcp.can_send() {
					flags |= EventFlags::WRITABLE;
				} else if !self.allow_write && !tcp.may_send() {
					flags |= EventFlags::WCLOSED;
				}
				if tcp.can_recv() {
					flags |= EventFlags::READABLE;
				} else if !self.allow_read && !tcp.may_recv() {
					flags |= EventFlags::RCLOSED;
				}
				EventFlags(flags)
			}),
			Inner::Backlog(ref backlog) => backlog
				.iter()
				.any(|handle| nic.with_ref(handle, |tcp: &TcpSocket| tcp.is_active()))
				.then(|| EventFlags(EventFlags::READABLE))
				.unwrap_or(EventFlags(EventFlags::NONE)),
		})
	}

	fn may_close(&self) -> bool {
		self.has_remaining_packets().map(bool::not).unwrap_or(true)
	}

	fn init_close(&mut self) {
		// forbid further read or write operations
		self.rclose();
		self.wclose();
	}

	fn close(&mut self) {
		self.with_socket_mut(|tcp| {
			Ok({
				tcp.close();
				//tcp.abort();
			})
		})
		.unwrap_or(());
	}
}

impl AsyncTcpSocket {
	pub(crate) fn new(mut local: IpEndpoint) -> Self {
		let handle = nic::lock().with(|nic| nic.create_tcp_handle());

		if local.port == 0 {
			local.port = crate::net::local_endpoint();
		}

		Self {
			inner: Inner::Handle(handle),
			local,
			allow_read: true,
			allow_write: true,
		}
	}

	pub(crate) fn poll_socket<F, T>(&self, f: F) -> io::Result<PollSocketRaw<F>>
	where
		F: FnMut(&TcpSocket) -> Poll<io::Result<T>>,
	{
		Ok(PollSocketRaw::new(self.inner.as_handle()?.clone(), f))
	}

	pub(crate) fn poll_sockets<F, T>(&self, f: F) -> io::Result<PollSocketsRaw<F>>
	where
		F: FnMut(&TcpSocket) -> Poll<io::Result<T>>,
	{
		Ok(PollSocketsRaw::new(self.inner.as_backlog_ref()?.clone(), f))
	}

	/// call a closure with a reference to the corresponding socket in the nic
	///
	/// this will lock the nic so don't aquire any other lock within the closure
	pub(crate) fn with_socket_ref<F, T>(&self, f: F) -> io::Result<T>
	where
		F: FnOnce(&TcpSocket) -> io::Result<T>,
	{
		nic::lock().with(|nic| nic.with_ref(self.inner.as_handle()?, f))
	}

	/// call a closure with a reference to the corresponding socket in the nic
	///
	/// this will lock the nic so don't aquire any other lock within the closure
	/// this will wake the nic afterwards since the closure may alter the socket
	///
	/// if the socket will not be modified use with_socket_mut instead to spare a wake
	pub(crate) fn with_socket_mut<F, T>(&self, f: F) -> io::Result<T>
	where
		F: FnOnce(&mut TcpSocket) -> io::Result<T>,
	{
		nic::lock().with(|nic| {
			let ret = nic.with_mut(self.inner.as_handle()?, f);
			nic.wake();
			ret
		})
	}

	pub(crate) fn wclose(&mut self) {
		self.allow_write = false;
	}

	pub(crate) fn rclose(&mut self) {
		self.allow_read = false;
	}

	pub(crate) fn read(&mut self, buffer: &mut [u8], peek: bool) -> io::Result<Option<usize>> {
		if !self.allow_read {
			Err(io::Error::new(
				io::ErrorKind::NotConnected,
				&"the sockets read side is closed",
			))
		} else {
			self.with_socket_mut(|tcp| match tcp {
				tcp if tcp.can_recv() && peek => tcp
					.peek_slice(buffer)
                    .map(Some)
					.map_err(|_err| {
						io::Error::new(
							io::ErrorKind::NotConnected,
							&"the sockets read side is closed",
						)
					}),
				tcp if tcp.can_recv() && !peek => tcp
					.recv_slice(buffer)
                    .map(Some)
					.map_err(|_err| {
						io::Error::new(
							io::ErrorKind::NotConnected,
							&"the sockets read side is closed",
						)
					}),
				tcp if !tcp.may_recv() => Ok(Some(0)),
				_ => Ok(None),
			})
		}
	}

	pub(crate) fn write(&mut self, buffer: &[u8]) -> io::Result<Option<usize>> {
		if !self.allow_write {
			Err(io::Error::new(
				io::ErrorKind::NotConnected,
				&"the sockets write side is closed",
			))
		} else {
			self.with_socket_mut(|tcp| match tcp {
				tcp if tcp.can_send() => tcp
                    .send_slice(buffer)
                    .map(Some)
                    .map_err(|_err| {
                        io::Error::new(
                            io::ErrorKind::NotConnected,
                            &"the sockets write side is closed",
                        )
                    }),
				tcp if !tcp.may_send() => Ok(Some(0)),
				_ => Ok(None),
			})
		}
	}

	pub(crate) fn accept(&mut self) -> io::Result<Option<AsyncTcpSocket>> {
		let local = self.local;
		let backlog = self.inner.as_backlog_mut()?;
		let mut index = None;

		nic::lock().with(|nic| {
			for (i, h) in backlog.iter().enumerate() {
				nic.with_ref(h, |tcp: &TcpSocket| {
					if tcp.may_recv() && tcp.may_send() {
						index = Some(i);
					}
				})
			}
		});

		if let Some(index) = index {
			let handle = backlog.remove(index);
			let new = Self::duplicate_handle(&handle);
			nic::lock()
				.with(|nic| nic.with_mut(&new, |tcp: &mut TcpSocket| tcp.listen(local).unwrap()));
			backlog.push(new);
			Ok(Some(Self {
				inner: Inner::Handle(handle),
				local,
				allow_write: true,
				allow_read: true,
			}))
		} else {
			Ok(None)
		}
	}

	pub(crate) fn set_hop_limit(&mut self, hop_limit: Option<u8>) -> io::Result<()> {
		self.with_socket_mut(|tcp| Ok(tcp.set_hop_limit(hop_limit)))
	}

	pub(crate) fn hop_limit(&self) -> io::Result<Option<u8>> {
		self.with_socket_ref(|tcp| Ok(tcp.hop_limit()))
	}

	pub(crate) fn local_addr(&self) -> IpEndpoint {
		self.local
	}

	pub(crate) fn connect(
		&mut self,
		remote: IpEndpoint,
	) -> io::Result<PollSocketRaw<impl FnMut(&TcpSocket) -> Poll<io::Result<()>>>> {
		let local = self.local;
		self.with_socket_mut(|tcp| match tcp {
			tcp if tcp.is_active()
				&& tcp.remote_endpoint().is_specified()
				&& tcp.remote_endpoint() == remote =>
			{
				Ok(())
			}
			tcp if tcp.is_active() && tcp.remote_endpoint().is_specified() => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"already in use",
			)),
			tcp if tcp.is_open() => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"already in use",
			)),
			_ => tcp
				.connect(remote, local)
				.map_err(|_err| io::Error::new(io::ErrorKind::InvalidData, &"invalid remote")),
		})
		.and_then(|()| self.poll_connected())
	}

	pub(crate) fn listen(&mut self, backlog: usize) -> io::Result<()> {
		let local = self.local;
		trace!("listen on {:?}", local);
		let vec = match self.inner {
			Inner::Backlog(_) => {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					&"already listening",
				))
			}
			Inner::Handle(ref handle) => {
				if nic::lock().with(|nic| nic.with_ref(handle, |tcp: &TcpSocket| tcp.is_open())) {
					return Err(io::Error::new(io::ErrorKind::InvalidInput, &"already open"));
				} else {
					(0..backlog)
						.map(|_| Self::duplicate_handle(handle))
						.collect()
				}
			}
		};
		for handle in &vec {
			nic::lock()
				.with(|nic| nic.with_mut(handle, |tcp: &mut TcpSocket| tcp.listen(local).unwrap()));
		}
		self.inner = Inner::Backlog(vec);
		Ok(())
	}

	pub(crate) fn remote_addr(&self) -> io::Result<IpEndpoint> {
		self.with_socket_ref(|tcp| {
			if tcp.is_active() {
				Ok(tcp.remote_endpoint())
			} else {
				Err(io::Error::new(
					io::ErrorKind::NotConnected,
					&"no endpoint available",
				))
			}
		})
	}

	pub(crate) fn poll_incoming_connection(
		&self,
	) -> io::Result<PollSocketsRaw<impl FnMut(&TcpSocket) -> Poll<io::Result<()>>>> {
		self.poll_sockets(|tcp| {
			if tcp.may_recv() && tcp.may_send() {
				trace!("found incoming connection");
				Poll::Ready(Ok(()))
			} else if !tcp.is_open() {
				warn!("incoming connection reset");
				Poll::Ready(Err(io::Error::new(
					io::ErrorKind::ConnectionReset,
					&"connection reset",
				)))
			} else {
				Poll::Pending(WakeOn::SendRecv)
			}
		})
	}

	pub(crate) fn poll_connected(
		&self,
	) -> io::Result<PollSocketRaw<impl FnMut(&TcpSocket) -> Poll<io::Result<()>>>> {
		self.poll_socket(|tcp| {
			info!("poll_connected. State: {:?}", tcp.state());
			if tcp.may_recv() && tcp.may_send() {
				Poll::Ready(Ok(()))
			} else if !tcp.is_active() {
				Poll::Ready(Err(io::Error::new(
					io::ErrorKind::NotConnected,
					&"connection refused",
				)))
			} else {
				Poll::Pending(WakeOn::Send)
			}
		})
	}

	pub(crate) fn poll_readable(
		&self,
	) -> io::Result<PollSocketRaw<impl FnMut(&TcpSocket) -> Poll<io::Result<()>>>> {
		self.poll_socket(move |tcp| {
			if tcp.can_recv() {
				Poll::Ready(Ok(()))
			} else if !tcp.is_active() {
				Poll::Ready(Err(io::Error::new(
					io::ErrorKind::NotConnected,
					&"not connected",
				)))
			} else if !tcp.may_recv() {
				Poll::Ready(Ok(()))
            } else {
				Poll::Pending(WakeOn::Recv)
			}
		})
	}

	pub(crate) fn poll_writable(
		&self,
	) -> io::Result<PollSocketRaw<impl FnMut(&TcpSocket) -> Poll<io::Result<()>>>> {
		self.poll_socket(|tcp| {
			if tcp.can_send() {
				Poll::Ready(Ok(()))
			} else if !tcp.is_active() {
				Poll::Ready(Err(io::Error::new(
					io::ErrorKind::NotConnected,
					&"not connected",
				)))
            } else if !tcp.may_send() {
				Poll::Ready(Ok(()))
			} else {
				Poll::Pending(WakeOn::Send)
			}
		})
	}

	pub(crate) fn has_remaining_packets(&self) -> io::Result<bool> {
		self.with_socket_ref(|tcp| {
			Ok(if tcp.may_send() && tcp.send_queue() > 0 {
				true
			} else {
				false
			})
		})
	}

	fn duplicate_handle(handle: &HandleWrapper) -> HandleWrapper {
		nic::lock().with(|nic| {
			let new = nic.create_tcp_handle();
			let hop_limit = nic.with_ref(handle, |tcp: &TcpSocket| tcp.hop_limit());
			nic.with_mut(&new, |tcp: &mut TcpSocket| tcp.set_hop_limit(hop_limit));
			new
		})
	}
}
