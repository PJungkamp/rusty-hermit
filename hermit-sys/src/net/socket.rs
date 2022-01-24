use crate::net::{nic, poll};
use futures_lite::future;
use hermit_abi::{io, net};
use smoltcp::socket::SocketHandle;
use std::task;

pub(crate) mod event;
pub(crate) mod tcp;
pub(crate) mod waker;

pub(crate) use event::AsyncEventSocket;
pub(crate) use tcp::AsyncTcpSocket;
pub(crate) use waker::AsyncWakerSocket;

use super::socket_map;

#[derive(Debug)]
pub(crate) struct HandleWrapper(SocketHandle);

impl HandleWrapper {
	pub(crate) fn new(handle: SocketHandle) -> Self {
		Self(handle)
	}

	pub(crate) fn inner(&self) -> SocketHandle {
		self.0
	}
}

impl Clone for HandleWrapper {
	fn clone(&self) -> Self {
		nic::lock().with(|nic| nic.socket_set.retain(self.inner()));
		Self::new(self.inner())
	}
}

impl Drop for HandleWrapper {
	fn drop(&mut self) { 
        nic::lock().with(|nic| nic.socket_set.release(self.inner()));
	}
}

pub(crate) trait SocketProxy<S> {
	fn with_ref<F, T>(&mut self, f: F) -> io::Result<T>
	where
		F: FnMut(&S) -> io::Result<T>;
	fn with_mut<F, T>(&mut self, f: F) -> io::Result<T>
	where
		F: FnMut(&mut S) -> io::Result<T>;
}

pub(crate) struct AsyncSocketProxy<R, M> {
	socket: net::Socket,
	as_ref: R,
	as_mut: M,
}

impl<R, M> AsyncSocketProxy<R, M> {
	pub(crate) fn new(socket: net::Socket, as_ref: R, as_mut: M) -> Self {
		Self {
			socket,
			as_ref,
			as_mut,
		}
	}
}

impl<S, R, M> SocketProxy<S> for AsyncSocketProxy<R, M>
where
	S: Socket,
	R: FnMut(&AsyncSocket) -> io::Result<&S>,
	M: FnMut(&mut AsyncSocket) -> io::Result<&mut S>,
{
	fn with_ref<F, T>(&mut self, f: F) -> io::Result<T>
	where
		F: FnOnce(&S) -> io::Result<T>,
	{
		let guard = socket_map::lock();
		let async_socket = &guard.get(self.socket)?.async_socket;
		let inner = (self.as_ref)(async_socket)?;
		f(inner)
	}

	fn with_mut<F, T>(&mut self, f: F) -> io::Result<T>
	where
		F: FnOnce(&mut S) -> io::Result<T>,
	{
		let mut guard = socket_map::lock();
		let async_socket = &mut guard.get_mut(self.socket)?.async_socket;
		let inner = (self.as_mut)(async_socket)?;
		f(inner)
	}
}

#[non_exhaustive]
#[derive(Debug)]
pub(crate) enum AsyncSocket {
	Unbound,
	Tcp(AsyncTcpSocket),
	Event(AsyncEventSocket),
	Waker(AsyncWakerSocket),
}

pub(crate) trait Socket {
	/// register a send waker (this waker is unique)
	fn register_exclusive_send_waker(
		&mut self,
		waker: &task::Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)>;
	/// register a recv waker (this waker is unique)
	fn register_exclusive_recv_waker(
		&mut self,
		waker: &task::Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)>;
	/// get the eventflags for this socket
	fn get_event_flags(&self, socket_map: &socket_map::SocketMap) -> net::event::EventFlags;
	/// update state on any wake event
	///
	/// by default this does nothing
	fn update(&mut self) {}
	/// tell the socket we want to close, but don't free resources yet
	///
	/// by default this does nothing
	fn init_close(&mut self) {}
	/// may we hard close this socket?
	///
	/// by default this returns true
	fn may_close(&self) -> bool {
		true
	}
	/// close the socket. now resources may be freed
	fn close(&mut self);
	/// has close finished
	///
	/// if we want to support something similar to SO_LINGER on unix
	/// we may want to wait until the socket is really closed
	///
	/// by default this returns true
	fn is_closed(&self) -> bool {
		true
	}
}

pub(crate) async fn close(socket: net::Socket) -> io::Result<()> {
	debug!("init_close()");
	socket_map::lock().get_mut(socket).and_then(|entry| {
		entry
			.async_socket
			.as_socket_mut()
			.map(|socket| socket.init_close())?;
		entry.closing = true;
		Ok(())
	})?;
	debug!("waiting for may_close()");
	future::poll_fn(|cx| {
		let mut guard = socket_map::lock();
		guard
			.get_mut(socket)
			.and_then(|entry| entry.async_socket.as_socket_mut())
			.map(|async_socket| async_socket.may_close())
			.map(|may_close| {
				if may_close {
					task::Poll::Ready(Ok(()))
				} else {
					guard.register_exclusive_waker(socket, cx.waker(), poll::WakeOn::SendRecv);
					task::Poll::Pending
				}
			})
			.unwrap_or_else(|err| task::Poll::Ready(Err(err)))
	})
	.await?;
	debug!("close()");
	socket_map::lock()
		.get_mut(socket)
		.and_then(|entry| entry.async_socket.as_socket_mut())
		.map(|socket| socket.close())?;
	debug!("waiting for is_closed()");
	future::poll_fn(|cx| {
		let mut guard = socket_map::lock();
		guard
			.get_mut(socket)
			.and_then(|entry| entry.async_socket.as_socket_mut())
			.map(|async_socket| async_socket.is_closed())
			.map(|is_closed| {
				if is_closed {
					task::Poll::Ready(Ok(()))
				} else {
					guard.register_exclusive_waker(socket, cx.waker(), poll::WakeOn::SendRecv);
					task::Poll::Pending
				}
			})
			.unwrap_or_else(|err| task::Poll::Ready(Err(err)))
	})
	.await?;
	let _ = socket_map::lock().take(socket);
	Ok(())
}

impl AsyncSocket {
	pub(crate) fn as_socket_ref(&self) -> io::Result<&dyn Socket> {
		match self {
			Self::Unbound => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"this socket is unbound",
			)),
			Self::Tcp(tcp) => Ok(tcp),
			Self::Event(event) => Ok(event),
			Self::Waker(waker) => Ok(waker),
		}
	}

	pub(crate) fn as_socket_mut(&mut self) -> io::Result<&mut dyn Socket> {
		match self {
			Self::Unbound => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"this socket is unbound",
			)),
			Self::Tcp(tcp) => Ok(tcp),
			Self::Event(event) => Ok(event),
			Self::Waker(waker) => Ok(waker),
		}
	}

	pub(crate) fn as_tcp_ref(&self) -> io::Result<&AsyncTcpSocket> {
		match self {
			Self::Tcp(tcp) => Ok(tcp),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp socket",
			)),
		}
	}

	pub(crate) fn as_tcp_mut(&mut self) -> io::Result<&mut AsyncTcpSocket> {
		match self {
			Self::Tcp(tcp) => Ok(tcp),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp socket",
			)),
		}
	}

	pub(crate) fn as_tcp_proxy(
		&self,
		socket: net::Socket,
	) -> io::Result<impl SocketProxy<AsyncTcpSocket>> {
		match self {
			Self::Tcp(_) => Ok(AsyncSocketProxy::new(
				socket,
				Self::as_tcp_ref,
				Self::as_tcp_mut,
			)),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp socket",
			)),
		}
	}

	pub(crate) fn as_event_ref(&self) -> io::Result<&AsyncEventSocket> {
		match self {
			Self::Event(event) => Ok(event),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp listener",
			)),
		}
	}

	pub(crate) fn as_event_mut(&mut self) -> io::Result<&mut AsyncEventSocket> {
		match self {
			Self::Event(event) => Ok(event),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp listener",
			)),
		}
	}

	pub(crate) fn as_event_proxy(
		&self,
		socket: net::Socket,
	) -> io::Result<impl SocketProxy<AsyncEventSocket>> {
		match self {
			Self::Event(_) => Ok(AsyncSocketProxy::new(
				socket,
				Self::as_event_ref,
				Self::as_event_mut,
			)),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not an event socket",
			)),
		}
	}

	pub(crate) fn as_waker_ref(&self) -> io::Result<&AsyncWakerSocket> {
		match self {
			Self::Waker(waker) => Ok(waker),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp listener",
			)),
		}
	}

	pub(crate) fn as_waker_mut(&mut self) -> io::Result<&mut AsyncWakerSocket> {
		match self {
			Self::Waker(waker) => Ok(waker),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not a tcp listener",
			)),
		}
	}

	pub(crate) fn as_waker_proxy(
		&self,
		socket: net::Socket,
	) -> io::Result<impl SocketProxy<AsyncWakerSocket>> {
		match self {
			Self::Waker(_) => Ok(AsyncSocketProxy::new(
				socket,
				Self::as_waker_ref,
				Self::as_waker_mut,
			)),
			_ => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				&"not an event socket",
			)),
		}
	}
}

impl From<AsyncTcpSocket> for AsyncSocket {
	fn from(socket: AsyncTcpSocket) -> Self {
		Self::Tcp(socket)
	}
}

impl From<AsyncEventSocket> for AsyncSocket {
	fn from(event: AsyncEventSocket) -> Self {
		Self::Event(event)
	}
}

impl From<AsyncWakerSocket> for AsyncSocket {
	fn from(waker: AsyncWakerSocket) -> Self {
		Self::Waker(waker)
	}
}
