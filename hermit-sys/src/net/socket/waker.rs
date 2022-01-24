use crate::net::socket_map;
use crate::net::waker::WakerRegistration;
use hermit_abi::net;
use hermit_abi::net::event::EventFlags;
use std::task::Waker;
use std::sync::atomic::{AtomicU32,Ordering};

#[derive(Debug)]
pub(crate) struct AsyncWakerSocket {
	event_flags: AtomicU32,
	send_waker: WakerRegistration,
	recv_waker: WakerRegistration,
}

impl super::Socket for AsyncWakerSocket {
	fn register_exclusive_send_waker(
		&mut self,
		waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		self.send_waker.register(waker);
		None
	}

	fn register_exclusive_recv_waker(
		&mut self,
		waker: &Waker,
	) -> Option<(Vec<net::Socket>, Vec<net::Socket>)> {
		self.recv_waker.register(waker);
		None
	}

	fn get_event_flags(&self, _: &socket_map::SocketMap) -> EventFlags {
		EventFlags(self.event_flags.swap(EventFlags::NONE, Ordering::SeqCst))
	}

	fn close(&mut self) {}
}

impl AsyncWakerSocket {
	pub(crate) fn new(socket: Option<net::Socket>) -> Self {
		Self {
			send_waker: WakerRegistration::new(),
			recv_waker: WakerRegistration::new(),
			event_flags: AtomicU32::new(EventFlags::NONE),
		}
	}

	fn wake_send(&mut self) {
		self.send_waker.wake();
	}

	fn wake_recv(&mut self) {
		self.recv_waker.wake();
	}

	pub(crate) fn close(&mut self) {
		self.wake_send();
		self.wake_recv();
	}

	pub(crate) fn send_event(&mut self, event_flags: EventFlags) {
		if event_flags.0 & (EventFlags::RCLOSED | EventFlags::READABLE) != EventFlags::NONE {
			self.wake_recv();
		}
		if event_flags.0 & (EventFlags::RCLOSED | EventFlags::READABLE) != EventFlags::NONE {
			self.wake_recv();
		}
		self.event_flags.store(event_flags.0,Ordering::SeqCst);
	}
}
