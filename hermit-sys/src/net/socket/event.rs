use crate::net::{poll, socket_map};
use hermit_abi::io;
use hermit_abi::net;
use hermit_abi::net::event::{Event, EventFlags};
use std::task::Waker;

#[derive(Debug, Clone)]
pub(crate) struct AsyncEventSocket {
	events: Vec<Event>,
}

impl super::Socket for AsyncEventSocket {
	fn register_exclusive_send_waker(
		&mut self,
		_waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		None
	}

	fn register_exclusive_recv_waker(
		&mut self,
		_waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		let mut send = Vec::new();
		let mut recv = Vec::new();
		for Event { socket, flags, .. } in self.events.iter() {
			if flags.0 & (EventFlags::WRITABLE | EventFlags::WCLOSED) != EventFlags::NONE {
				send.push(*socket);
			}
			if flags.0 & (EventFlags::READABLE | EventFlags::RCLOSED) != EventFlags::NONE {
				recv.push(*socket);
			}
		}
		Some((send, recv))
	}

	fn get_event_flags(&self, socket_map: &socket_map::SocketMap) -> EventFlags {
		self.events
			.iter()
			.any(|Event { socket, flags, .. }| {
				socket_map
					.get(*socket)
					.and_then(|entry| entry.async_socket.as_socket_ref())
					.map(|socket| socket.get_event_flags(socket_map).0 & flags.0 != EventFlags::NONE)
					.unwrap_or(false)
			})
			.then(|| EventFlags(EventFlags::READABLE))
			.unwrap_or(EventFlags(EventFlags::NONE))
	}

	fn close(&mut self) {}
}

impl AsyncEventSocket {
	pub(crate) fn new() -> Self {
		Self { events: Vec::new() }
	}

	pub(crate) fn add_event(&mut self, event: Event) -> io::Result<()> {
		self.events
			.iter()
			.find(|e| e.socket == event.socket)
			.is_none()
			.then(|| self.events.push(event))
			.ok_or(io::Error::new(
				io::ErrorKind::AlreadyExists,
				&"an event for the specified socket already exists",
			))
	}

	pub(crate) fn modify_event(&mut self, event: Event) -> io::Result<()> {
		self.events
			.iter_mut()
			.find(|e| e.socket == event.socket)
			.map(|e| *e = event)
			.ok_or(io::Error::new(
				io::ErrorKind::NotFound,
				&"this socket is not part of the specified event socket",
			))
	}

	pub(crate) fn remove_socket(&mut self, socket: net::Socket) -> io::Result<()> {
		self.events
			.iter()
			.enumerate()
			.find_map(|(index, e)| (e.socket == socket).then(|| index))
			.map(|index| self.events.remove(index))
			.map(|_| ())
			.ok_or(io::Error::new(
				io::ErrorKind::NotFound,
				&"this socket is not part of the specified event socket",
			))
	}

	pub(crate) fn poll_events(
		&self,
	) -> poll::PollEventsRaw<impl FnMut(&Event) -> poll::Poll<io::Result<Event>>> {
		poll::PollEventsRaw::new(self.events.clone(), move |event| {
			let guard = socket_map::lock();
            guard
				.get(event.socket)
				.and_then(|entry| {
					let flags = entry.async_socket.as_socket_ref()?.get_event_flags(&guard);
					debug!("flags are: {:b}", flags.0);
					debug!("interests are: {:b}", event.flags.0);
					let matching_flags = flags.0 & event.flags.0;
					if matching_flags != EventFlags::NONE {
						Ok(poll::Poll::Ready(Ok(Event {
							flags: EventFlags(matching_flags),
							..*event
						})))
					} else {
						match event.flags.0 {
							f if f & (EventFlags::READABLE | EventFlags::RCLOSED) != 0 => {
								Ok(poll::Poll::Pending(poll::WakeOn::Recv))
							}
							f if f & (EventFlags::WRITABLE | EventFlags::WCLOSED) != 0 => {
								Ok(poll::Poll::Pending(poll::WakeOn::Recv))
							}
							_ => unreachable!(),
						}
					}
				})
				.unwrap_or_else(|_err| match event.flags.0 {
					f if f & (EventFlags::READABLE | EventFlags::RCLOSED) != 0 => {
						poll::Poll::Ready(Ok(Event {
							flags: EventFlags(EventFlags::RCLOSED),
							..*event
						}))
					}
					f if f & (EventFlags::WRITABLE | EventFlags::WCLOSED) != 0 => {
						poll::Poll::Ready(Ok(Event {
							flags: EventFlags(EventFlags::WCLOSED),
							..*event
						}))
					}
					_ => unreachable!(),
				})
		})
	}
}
