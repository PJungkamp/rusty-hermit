pub mod device;
pub mod executor;
pub mod nic;
pub mod socket;
pub mod socket_map;
pub mod poll;
pub mod waker;

#[cfg(target_arch = "aarch64")]
use aarch64::regs::*;
use hermit_abi::net::event::{Event, EventFlags};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_rdtsc;
use std::convert::TryInto;
use std::mem::MaybeUninit;
use std::str::FromStr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::task::Poll;
use std::time::Duration;
use std::u16;

mod smol {
	#[cfg(feature = "dhcpv4")]
	pub use smoltcp::dhcp::Dhcpv4Client;
	#[cfg(feature = "trace")]
	pub use smoltcp::phy::EthernetTracer;
	#[cfg(feature = "dhcpv4")]
	pub use smoltcp::wire::{IpCidr, Ipv4Cidr};

	pub use smoltcp::iface::EthernetInterface;
	pub use smoltcp::phy::Device;
	pub use smoltcp::socket::{
		Socket, SocketHandle, SocketSet, TcpSocket, TcpSocketBuffer, TcpState,
	};
	pub use smoltcp::time::{Duration, Instant};
	pub use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address, IpEndpoint};
	pub use smoltcp::Error;
}

use hermit_abi::io;
use hermit_abi::net as abi;
use hermit_abi::Tid;
use futures_lite::future;
use crate::net::device::HermitNet;
use crate::net::executor::{block_on, run_executor, spawn};
use crate::net::socket::AsyncTcpSocket;

use self::socket::SocketProxy;

macro_rules! abi_to_smol {
    ($expr:expr => IpEndpoint) => {
        match $expr {
            abi::SocketAddr::V4(addr) => {
                let ip = abi_to_smol!(addr.ip_addr => Ipv4Address);
                smol::IpEndpoint {
                    addr: ip,
                    port: addr.port
                }
            },
            abi::SocketAddr::V6(addr) => {
                let ip = abi_to_smol!(addr.ip_addr => Ipv6Address);
                smol::IpEndpoint {
                    addr: ip,
                    port: addr.port
                }
            },
        }
    };
    ($expr:expr => IpAddress) => {
        match $expr {
            abi::IpAddr::V4(ip) => abi_to_smol!(ip => Ipv4Address),
            abi::IpAddr::V6(ip) => abi_to_smol!(ip => Ipv6Address),
        }
    };
    ($expr:expr => Ipv4Address) => {
        {
            let ip = $expr;
            smol::IpAddress::v4(ip.a ,ip.b ,ip.c ,ip.d)
        }
    };
    ($expr:expr => Ipv6Address) => {
        {
            let ip = $expr;
            smol::IpAddress::v6(ip.a, ip.b, ip.c, ip.d, ip.e, ip.f, ip.g, ip.h)
        }
    }
}

macro_rules! smol_to_abi {
    ($expr:expr => SocketAddr) => {
        {
            let smol::IpEndpoint { addr, port } = $expr;
            match addr {
                smol::IpAddress::Ipv4(ipv4) => abi::SocketAddr::V4(abi::SocketAddrV4{
                    ip_addr: smol_to_abi!(ipv4 => Ipv4Addr), 
                    port,
                }),
                smol::IpAddress::Ipv6(ipv6) => abi::SocketAddr::V6(abi::SocketAddrV6 {
                    ip_addr: smol_to_abi!(ipv6 => Ipv6Addr), 
                    port,
                    flowinfo: 0,
                    scope_id: 0,
                }),
                _ => abi::SocketAddr::V4(abi::SocketAddrV4::UNSPECIFIED),
            }
        }
    };
    ($expr:expr => IpAddr) => {
        match addr {
            smol::IpAddress::Unspecified => abi::IpAddr::V4(abi::SocketAddrV4::UNSPECIFIED),
            smol::IpAddress::Ipv4(ipv4) => abi::IpAddr::V4(smol_to_abi!(ipv4 => Ipv4Address)),
            smol::IpAddress::Ipv6(ipv6) => abi::IpAddr::V6(smol_to_abi!(ipv6 => Ipv6Address)),
        }
    };
    ($expr:expr => Ipv4Addr) => {
        {
            let smol::Ipv4Address([a,b,c,d]) = $expr;
            abi::Ipv4Addr { a,b,c,d }
        }
    };
    ($expr:expr => Ipv6Addr) => {
        {
            let mut buffer = [0u16;8];
            let ip: smol::Ipv6Address = $expr;
            ip.write_parts(&mut buffer);
            let [a,b,c,d,e,f,g,h] = buffer;
            abi::Ipv6Addr { a,b,c,d,e,f,g,h }
        }
    }
}

extern "C" {
	fn sys_yield();
	fn sys_spawn(
		id: *mut Tid,
		func: extern "C" fn(usize),
		arg: usize,
		prio: u8,
		selector: isize,
	) -> i32;
	fn sys_netwait();
}

static LOCAL_ENDPOINT: AtomicU16 = AtomicU16::new(0);

fn local_endpoint() -> u16 {
	LOCAL_ENDPOINT.fetch_add(1, Ordering::Acquire) | 0xC000
}

#[cfg(target_arch = "x86_64")]
fn start_endpoint() -> u16 {
	((unsafe { _rdtsc() as u64 }) % (u16::MAX as u64))
		.try_into()
		.unwrap()
}

#[cfg(target_arch = "aarch64")]
fn start_endpoint() -> u16 {
	(CNTPCT_EL0.get() % (u16::MAX as u64)).try_into().unwrap()
}

pub(crate) fn network_delay(timestamp: smol::Instant) -> Option<smol::Duration> {
	nic::lock().with(|nic| nic.poll_delay(timestamp))
}

pub(crate) async fn network_run() {
	let _: () = future::poll_fn(|cx| {
		trace!("network_run");
		nic::lock().with(|nic| {
			nic.socket_set.prune();
			nic.poll(cx, smol::Instant::now());
		});
		Poll::Pending
	})
	.await;
}

extern "C" fn nic_thread(_: usize) {
	loop {
		unsafe {
			sys_netwait();
		}

		// wake the network_run future on the executor
		trace!("waking nic");
		nic::lock().with(|nic| nic.wake());
		// this wakes all threads that have blocking io ready
		run_executor();
	}
}

pub(crate) fn network_init() -> io::Result<()> {
	// initialize variable, which contains the next local endpoint
	LOCAL_ENDPOINT.store(start_endpoint(), Ordering::Release);

	let mut guard = nic::lock();
	*guard = nic::NetworkInterface::<HermitNet>::new();

	guard
		.with(|nic| Ok(nic.poll_delay(smoltcp::time::Instant::now())))
		.and_then(|_| {
			spawn(network_run()).detach();

			// create thread, which manages the network stack
			// use a higher priority to reduce the network latency
			let mut tid: Tid = 0;
			let ret = unsafe { sys_spawn(&mut tid, nic_thread, 0, 3, 0) };
			if ret >= 0 {
				debug!("Spawn network thread with id {}", tid);
			}

			// switch to network thread
			unsafe { sys_yield() };
			Ok(())
		})
}

///////////////////////////////////////////////////////////////////
// new interface
///////////////////////////////////////////////////////////////////

/// creates a new socket (TCP/UDP/...)
#[no_mangle]
pub fn sys_socket() -> io::Result<abi::Socket> {
	let socket = socket_map::lock().new_socket(socket_map::Options {
		non_blocking: false,
		timeout: None,
	});
	run_executor();
	Ok(socket)
}

/// makes the socket non_blocking
#[no_mangle]
pub fn sys_socket_set_timeout(socket: abi::Socket, timeout: Option<Duration>) -> io::Result<()> {
	socket_map::lock().get_mut(socket)?.options.timeout =
		timeout.map(|duration| smol::Duration::from(duration));
	Ok(())
}

#[no_mangle]
pub fn sys_socket_timeout(socket: abi::Socket) -> io::Result<Option<Duration>> {
	Ok(socket_map::lock().get_mut(socket)?.options.timeout
       .map(|timeout| timeout.into()))
}

/// makes the socket non_blocking
#[no_mangle]
pub fn sys_socket_set_non_blocking(socket: abi::Socket, non_blocking: bool) -> io::Result<()> {
	socket_map::lock().get_mut(socket)?.options.non_blocking = non_blocking;
	Ok(())
}

#[no_mangle]
pub fn sys_socket_non_blocking(socket: abi::Socket) -> io::Result<bool> {
	Ok(socket_map::lock().get_mut(socket)?.options.non_blocking)
}

/// close a socket
#[no_mangle]
pub fn sys_socket_close(socket: abi::Socket) -> io::Result<()> {
	run_executor();
    let options = { 
        let mut guard = socket_map::lock();
        let (async_socket, options) = guard
            .get_mut(socket)?
            .split_mut();
        let async_socket = match async_socket.as_socket_mut() {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        async_socket.init_close();
        *options
    };
	if options.non_blocking {
		spawn(socket::close(socket)).detach();
	} else {
		block_on(socket::close(socket), None)?;
	}
	Ok(())
}

// ---- event ----

#[no_mangle]
pub fn sys_event_bind(socket: abi::Socket) -> io::Result<()> {
	debug!("binding event socket");
	socket_map::lock().bind_socket(socket, socket::AsyncEventSocket::new().into())?;
	run_executor();
	Ok(())
}

#[no_mangle]
pub fn sys_event_add(socket: abi::Socket, event: abi::event::Event) -> io::Result<()> {
	socket_map::lock()
		.get_mut(socket)?
		.async_socket
		.as_event_mut()?
		.add_event(event)
}

#[no_mangle]
pub fn sys_event_modify(socket: abi::Socket, event: abi::event::Event) -> io::Result<()> {
	socket_map::lock()
		.get_mut(socket)?
		.async_socket
		.as_event_mut()?
		.modify_event(event)
}

#[no_mangle]
pub fn sys_event_remove(socket: abi::Socket, target: abi::Socket) -> io::Result<()> {
	socket_map::lock()
		.get_mut(socket)?
		.async_socket
		.as_event_mut()?
		.remove_socket(target)
}

#[no_mangle]
pub fn sys_event_wait(socket: abi::Socket, events: &mut [MaybeUninit<Event>]) -> io::Result<usize> {
    let (mut proxy, options) = {
        let guard = socket_map::lock();
        let (async_socket, options) = guard
            .get(socket)?
            .split_ref();
        let proxy = async_socket
            .as_event_proxy(socket)?;
        (proxy,*options)
    };

    let event = proxy
        .with_ref(|async_socket| Ok(
            async_socket
                .poll_events()
                .with(socket,options.into())
        ))?
        .execute()?;

    if let Some((head,tail)) = events.split_first_mut() {
        head.write(event);
        let count = events
            .iter_mut()
            .zip(std::iter::from_fn(move || proxy
                .with_ref(|async_socket| Ok(async_socket
                    .poll_events()
                    .with(socket,options.into())
                ))
                .and_then(|mut poll| poll.poll_once())
                .ok()
            ))
            .map(|(m,e)| m.write(e))
            .count();
        Ok(count + 1)
    } else {
        Ok(0)
    }
}

// ---- waker ----

#[no_mangle]
pub fn sys_waker_bind(socket: abi::Socket) -> io::Result<()> {
	debug!("binding waker socket");
	socket_map::lock().bind_socket(socket, socket::AsyncWakerSocket::new(None).into())?;
	run_executor();
	Ok(())
}

#[no_mangle]
pub fn sys_waker_send_event(socket: abi::Socket, flags: EventFlags) -> io::Result<()> {
	socket_map::lock()
		.get_mut(socket)?
		.async_socket
		.as_waker_mut()?
		.send_event(flags);
	run_executor();
	Ok(())
}

// --- tcp ---

#[no_mangle]
pub fn sys_tcp_bind(socket: abi::Socket, local: abi::SocketAddr) -> io::Result<()> {
	debug!("binding tcp socket");
	let local = abi_to_smol!(local => IpEndpoint);
	let handle = nic::lock().with(|nic| {
		if local.addr.is_unspecified() || nic.iface.has_ip_addr(local.addr) {
			Ok(nic.create_tcp_handle())
		} else {
			Err(io::Error::new(
				io::ErrorKind::AddrNotAvailable,
				&"specified address not available",
			))
		}
	})?;
	socket_map::lock()
        .bind_socket(socket, AsyncTcpSocket::new(local).into())?;
	run_executor();
	Ok(())
}

#[no_mangle]
pub fn sys_tcp_set_hop_limit(socket: abi::Socket, hop_limit: Option<u8>) -> io::Result<()> {
	socket_map::lock()
        .get_mut(socket)?
        .split_mut().0
        .as_tcp_mut()?
	    .set_hop_limit(hop_limit)
}

#[no_mangle]
pub fn sys_tcp_hop_limit(socket: abi::Socket) -> io::Result<Option<u8>> {
	socket_map::lock()
        .get(socket)?
        .split_ref().0
        .as_tcp_ref()?
	    .hop_limit()
}

#[no_mangle]
pub fn sys_tcp_local_addr(socket: abi::Socket) -> io::Result<abi::SocketAddr> {
	let ip_endpoint = socket_map::lock()
        .get(socket)?
        .split_ref().0
        .as_tcp_ref()?
	    .local_addr();
    Ok(smol_to_abi!(ip_endpoint => SocketAddr))
}

#[no_mangle]
pub fn sys_tcp_remote_addr(socket: abi::Socket) -> io::Result<abi::SocketAddr> {
	let ip_endpoint = socket_map::lock()
        .get(socket)?
        .split_ref().0
        .as_tcp_ref()?
	    .remote_addr()?;
    Ok(smol_to_abi!(ip_endpoint => SocketAddr))
}

/// make a socket listen for a connections
#[no_mangle]
pub fn sys_tcp_listen(socket: abi::Socket, backlog: usize) -> io::Result<()> {
	socket_map::lock()
        .get_mut(socket)?
        .split_mut().0
        .as_tcp_mut()?
	    .listen(backlog)?;
    run_executor();
    Ok(())
}

#[no_mangle]
pub fn sys_tcp_accept(socket: abi::Socket) -> io::Result<abi::Socket> {
	let (mut proxy,options) = {
        let guard = socket_map::lock();
        let (async_socket,options) = guard
                .get(socket)?
                .split_ref();
        let proxy = async_socket
            .as_tcp_proxy(socket)?;
        (proxy,*options)
    };

    loop {
        proxy
            .with_ref(|async_socket| Ok(
                async_socket
                    .poll_incoming_connection()?
                    .with(socket,options.into())
            ))?
	        .execute()?;
        match proxy.with_mut(|async_socket| async_socket.accept())? {
            Some(async_socket) => break {
                let mut guard = socket_map::lock();
                let socket = guard.new_socket(options);
                guard.bind_socket(socket,async_socket.into());
                Ok(socket)
            },
            None => continue,
        }
    }
}

#[no_mangle]
pub fn sys_tcp_connect(socket: abi::Socket, remote: abi::SocketAddr) -> io::Result<()> {
	let poll_connected = {
        let mut guard = socket_map::lock();
        let (async_socket,options) = guard
                .get_mut(socket)?
                .split_mut();
        let poll_connected = async_socket
            .as_tcp_mut()?
            .connect(abi_to_smol!(remote => IpEndpoint))?
            .with(socket,(*options).into());
        poll_connected
    };

    poll_connected.execute()
}

#[no_mangle]
pub fn sys_tcp_shutdown(socket: abi::Socket, mode: abi::Shutdown) -> io::Result<()> {
	run_executor();
    let mut guard = socket_map::lock();
    let (async_socket,_) = guard.get_mut(socket)?.split_mut();
    let async_socket = async_socket.as_tcp_mut()?;
	match mode {
		abi::Shutdown::Read => {
			async_socket.rclose();
			// nothing to do since this just inhibits read operations on the socket
			Ok(())
		}
		abi::Shutdown::Write => {
			async_socket.wclose();
			Ok(())
		}
		abi::Shutdown::Both => {
			async_socket.rclose();
			async_socket.wclose();
			Ok(())
		}
	}
}

#[no_mangle]
pub fn sys_tcp_write(socket: abi::Socket, buf: &[u8]) -> Result<usize, io::Error> {
	let (mut proxy,options) = {
        let guard = socket_map::lock();
        let (async_socket,options) = guard
                .get(socket)?
                .split_ref();
        let proxy = async_socket
            .as_tcp_proxy(socket)?;
        (proxy,*options)
    };

    loop {
        proxy
            .with_ref(|async_socket| Ok(async_socket
                .poll_writable()?
                .with(socket,options.into())
            ))?
            .execute()?;
        match proxy.with_mut(|async_socket| async_socket.write(buf))? {
            Some(n) => break Ok(n),
            None => continue,
        }
    }
}

#[no_mangle]
pub fn sys_tcp_read(socket: abi::Socket, buf: &mut [u8]) -> io::Result<usize> {
	let (mut proxy,options) = {
        let guard = socket_map::lock();
        let (async_socket,options) = guard
                .get(socket)?
                .split_ref();
        let proxy = async_socket
            .as_tcp_proxy(socket)?;
        (proxy,*options)
    };

    loop {
        proxy
            .with_ref(|async_socket| Ok(async_socket
                .poll_readable()?
                .with(socket,options.into())
            ))?
            .execute()?;
        match proxy.with_mut(|async_socket| async_socket.read(buf,false))? {
            Some(n) => break Ok(n),
            None => continue,
        }
    }
}

#[no_mangle]
pub fn sys_tcp_peek(socket: abi::Socket, buf: &mut [u8]) -> io::Result<usize> {
	let (mut proxy,options) = {
        let guard = socket_map::lock();
        let (async_socket,options) = guard
                .get(socket)?
                .split_ref();
        let proxy = async_socket
            .as_tcp_proxy(socket)?;
        (proxy,*options)
    };

    loop {
        proxy
            .with_ref(|async_socket| Ok(async_socket
                .poll_readable()?
                .with(socket,options.into())
            ))?
            .execute()?;
        match proxy.with_mut(|async_socket| 
            async_socket.read(buf,true))? 
        {
            Some(n) => break Ok(n),
            None => continue,
        }
    }
}

///////////////////////////////////////////////////////////////////
// deprecated interface
///////////////////////////////////////////////////////////////////

#[no_mangle]
pub fn sys_tcp_stream_connect(
	_ip_slice: &[u8],
	_port: u16,
	_timeout: Option<u64>,
) -> Result<smol::SocketHandle, ()> {
    Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_read(_handle: smol::SocketHandle, _buffer: &mut [u8]) -> Result<usize, ()> {
    Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_write(handle: smol::SocketHandle, buffer: &[u8]) -> Result<usize, ()> {
    Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_close(handle: smol::SocketHandle) -> Result<(), ()> {
    Err(())
}

//ToDo: an enum, or at least constants would be better
#[no_mangle]
pub fn sys_tcp_stream_shutdown(handle: smol::SocketHandle, how: i32) -> Result<(), ()> {
    Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_set_read_timeout(
	_handle: smol::SocketHandle,
	_timeout: Option<u64>,
) -> Result<(), ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_get_read_timeout(_handle: smol::SocketHandle) -> Result<Option<u64>, ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_set_write_timeout(
	_handle: smol::SocketHandle,
	_timeout: Option<u64>,
) -> Result<(), ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_get_write_timeout(_handle: smol::SocketHandle) -> Result<Option<u64>, ()> {
	Err(())
}

#[deprecated(since = "0.1.14", note = "Please don't use this function")]
#[no_mangle]
pub fn sys_tcp_stream_duplicate(_handle: smol::SocketHandle) -> Result<smol::SocketHandle, ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_peek(_handle: smol::SocketHandle, _buf: &mut [u8]) -> Result<usize, ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_set_nonblocking(_handle: smol::SocketHandle, _mode: bool) -> Result<(), ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_set_tll(_handle: smol::SocketHandle, _ttl: u32) -> Result<(), ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_get_tll(_handle: smol::SocketHandle) -> Result<u32, ()> {
	Err(())
}

#[no_mangle]
pub fn sys_tcp_stream_peer_addr(handle: smol::SocketHandle) -> Result<(smol::IpAddress, u16), ()> {
    Err(())
}

#[no_mangle]
pub fn sys_tcp_listener_accept(
	port: u16,
) -> Result<(smol::SocketHandle, smol::IpAddress, u16), ()> {
    Err(())
}
