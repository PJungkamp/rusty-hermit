use crate::net::device::HermitNet;
use crate::net::socket::HandleWrapper;
#[cfg(feature = "dhcpv4")]
use smoltcp::dhcp::Dhcpv4Client;
use smoltcp::iface::EthernetInterface;
use smoltcp::phy::Device;
use smoltcp::socket::{AnySocket, SocketSet, TcpSocket, TcpSocketBuffer};
#[cfg(feature = "dhcpv4")]
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};
use std::sync::{Mutex, MutexGuard};

lazy_static! {
	pub(crate) static ref NIC: Mutex<NetworkState> = Mutex::new(NetworkState::Missing);
}

/// lock the global NetworkState
///
/// This will panic if the network mutex is poisoned
pub(crate) fn lock() -> MutexGuard<'static, NetworkState> {
	NIC.lock().expect("Network State poisoned")
}

pub(crate) enum NetworkState {
	Missing,
	InitializationFailed,
	Initialized(NetworkInterface<HermitNet>),
}

impl NetworkState {
	pub(crate) fn with<F, R>(&mut self, f: F) -> R
	where
		F: FnOnce(&mut NetworkInterface<HermitNet>) -> R,
	{
		match self {
			NetworkState::Initialized(nic) => f(nic),
			_ => panic!("Network Interface not found"),
		}
	}
}

pub(crate) struct NetworkInterface<T: for<'a> Device<'a>> {
	#[cfg(feature = "trace")]
	pub iface: EthernetInterface<'static, EthernetTracer<T>>,
	#[cfg(not(feature = "trace"))]
	pub iface: EthernetInterface<'static, T>,
	pub socket_set: SocketSet<'static>,
	#[cfg(feature = "dhcpv4")]
	pub dhcp: Dhcpv4Client,
	#[cfg(feature = "dhcpv4")]
	pub prev_cidr: Ipv4Cidr,
	pub woken: bool,
}

impl<T> NetworkInterface<T>
where
	T: for<'a> Device<'a>,
{
	pub(crate) fn with_ref<S: AnySocket<'static>, F, R>(
		&mut self,
		handle: &HandleWrapper,
		f: F,
	) -> R
	where
		F: FnOnce(&S) -> R,
	{
		let socket = self.socket_set.get::<S>(handle.inner());
		f(&*socket)
	}

	pub(crate) fn with_mut<S: AnySocket<'static>, F, R>(
		&mut self,
		handle: &HandleWrapper,
		f: F,
	) -> R
	where
		F: FnOnce(&mut S) -> R,
	{
		let mut socket = self.socket_set.get::<S>(handle.inner());
		f(&mut *socket)
	}

	pub(crate) fn create_tcp_handle(&mut self) -> HandleWrapper {
		let tcp_rx_buffer = TcpSocketBuffer::new(vec![0; 2usize.pow(16)]);
		let tcp_tx_buffer = TcpSocketBuffer::new(vec![0; 2usize.pow(16)]);
		let tcp_socket = TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);
		trace!("creating tcp handle");
		let handle = self.socket_set.add(tcp_socket);
		HandleWrapper::new(handle)
	}

	pub(crate) fn wake(&mut self) {
		// wake the network future
		trace!(
			"waking nic with {} sockets in socket_set",
			self.socket_set.iter().count()
		);
		self.woken = true;
	}

	pub(crate) fn was_woken(&self) -> bool {
		self.woken
	}

	pub fn poll(&mut self, timestamp: smoltcp::time::Instant) {
        self.socket_set.prune();
		loop { 
            match self.iface.poll(&mut self.socket_set, timestamp.into()) {
                Ok(true) => (),
                Ok(false) => break,
                Err(err) => debug!("poll error: {:?}",err),
            }
        }
		#[cfg(feature = "dhcpv4")]
		let config = self
			.dhcp
			.poll(&mut self.iface, &mut self.socket_set, timestamp)
			.unwrap_or_else(|e| {
				debug!("DHCP: {:?}", e);
				None
			});
		#[cfg(feature = "dhcpv4")]
		config.map(|config| {
			debug!("DHCP config: {:?}", config);
			if let Some(cidr) = config.address {
				if cidr != self.prev_cidr && !cidr.address().is_unspecified() {
					self.iface.update_ip_addrs(|addrs| {
						addrs.iter_mut().next().map(|addr| {
							*addr = IpCidr::Ipv4(cidr);
						});
					});
					self.prev_cidr = cidr;
					debug!("Assigned a new IPv4 address: {}", cidr);
				}
			}

			config.router.map(|router| {
				self.iface
					.routes_mut()
					.add_default_ipv4_route(router)
					.unwrap()
			});
			self.iface.routes_mut().update(|routes_map| {
				routes_map
					.get(&IpCidr::new(Ipv4Address::UNSPECIFIED.into(), 0))
					.map(|default_route| {
						debug!("Default gateway: {}", default_route.via_router);
					});
			});

			if config.dns_servers.iter().any(|s| s.is_some()) {
				debug!("DNS servers:");
				for dns_server in config.dns_servers.iter().filter_map(|s| *s) {
					debug!("- {}", dns_server);
				}
			}
		});
		self.woken = false;
	}

	pub(crate) fn poll_delay(
		&mut self,
		timestamp: smoltcp::time::Instant,
	) -> Option<smoltcp::time::Duration> {
		self.iface.poll_delay(&self.socket_set, timestamp)
	}
}
