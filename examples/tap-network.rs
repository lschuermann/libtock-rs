#![no_main]
#![no_std]
use core::fmt::Write;
use libtock::console::Console;
use libtock::runtime::{set_main, stack_size, TockSyscalls};
use libtock::alarm::{Alarm, Milliseconds};

use libtock_platform as platform;
use libtock_platform::allow_ro::AllowRo;
use libtock_platform::allow_rw::AllowRw;
use libtock_platform::share;
use libtock_platform::subscribe::Subscribe;
use libtock_platform::{DefaultConfig, ErrorCode, Syscalls, YieldNoWaitReturn};

use core::cell::Cell;
use core::marker::PhantomData;

use smoltcp;

set_main! {main}
stack_size! {0x8000}

/// System call configuration trait for `TapNetwork`.
pub trait Config:
    platform::allow_ro::Config + platform::allow_rw::Config + platform::subscribe::Config
{
}
impl<T: platform::allow_ro::Config + platform::allow_rw::Config + platform::subscribe::Config>
    Config for T
{
}

const DRIVER_NUM: u32 = 0x30003;

struct TockTapRxToken<'a, const ETH_MTU: usize>(&'a mut [u8; ETH_MTU]);
impl<'a, const ETH_MTU: usize> smoltcp::phy::RxToken for TockTapRxToken<'a, ETH_MTU> {
    fn consume<R, F>(self, timestamp: smoltcp::time::Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        f(self.0)
    }
}

struct TockTapTxToken<'a, const ETH_MTU: usize, S: Syscalls, C: Config>(&'a mut [u8; ETH_MTU], PhantomData<(S, C)>);
impl<'a, const ETH_MTU: usize, S: Syscalls, C: Config> smoltcp::phy::TxToken for TockTapTxToken<'a, ETH_MTU, S, C> {
    fn consume<R, F>(
        self,
        timestamp: smoltcp::time::Instant,
        len: usize,
        f: F,
    ) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
	writeln!(Console::writer(), "smoltcp tx token consume {}", len).unwrap();

	// Fill the held buffer with the packet to send.
	let smoltcp_res = f(self.0)?;

	// Now, share this frame with the kernel and send it:
	let called: Cell<Option<()>> = Cell::new(None);
	share::scope::<(AllowRo<_, DRIVER_NUM, 0>, Subscribe<_, DRIVER_NUM, 2>), _, _>(
            |handle| -> Result<(), ErrorCode> {
                let (allow_ro, subscribe) = handle.split();

		S::subscribe::<_, _, C, DRIVER_NUM, 2>(subscribe, &called)?;

                S::allow_ro::<C, DRIVER_NUM, 0>(allow_ro, self.0)?;

		S::command(DRIVER_NUM, 6, len as u32, 0).to_result::<(), ErrorCode>()?;

		loop {
                    S::yield_wait();
                    if let Some(()) = called.get() {
			return Ok(());
                    }
                }
            },
        ).unwrap();

	Ok(smoltcp_res)
    }
}

struct TockTapDevice<const ETH_MTU: usize, S: Syscalls, C: Config = DefaultConfig> {
    syscalls: PhantomData<S>,
    config: PhantomData<C>,
    rx_buffer: [u8; ETH_MTU],
    rx_packet: Cell<Option<u16>>,
    tx_buffer: [u8; ETH_MTU],
}

impl<const ETH_MTU: usize, S: Syscalls, C: Config> TockTapDevice<ETH_MTU, S, C> {
    pub fn new() -> Self {
	S::command(DRIVER_NUM, 1, 0, 0).to_result::<(), ErrorCode>().unwrap();

        TockTapDevice {
            syscalls: PhantomData,
            config: PhantomData,
            rx_buffer: [0; ETH_MTU],
            rx_packet: Cell::new(None),
            tx_buffer: [0; ETH_MTU],
        }
    }

    pub fn try_receive(&mut self, block: bool) -> Result<bool, ErrorCode> {
	// writeln!(Console::writer(), "try_receive, block? {:?}", block).unwrap();
        if self.rx_packet.get().is_some() {
	    // writeln!(Console::writer(), "rx_packet == Some({})", self.rx_packet.get().unwrap()).unwrap();
            return Ok(true);
        }

        let called: Cell<Option<(u32,)>> = Cell::new(None);

        let packet_received =
            share::scope::<(AllowRw<_, DRIVER_NUM, 0>, Subscribe<_, DRIVER_NUM, 1>), _, _>(
                |handle| -> Result<Option<u16>, ErrorCode> {
                    let (allow_rw, subscribe) = handle.split();

		    S::subscribe::<_, _, C, DRIVER_NUM, 1>(subscribe, &called)?;

                    S::allow_rw::<C, DRIVER_NUM, 0>(allow_rw, &mut self.rx_buffer)?;

		    let packet_recv: u32 = S::command(DRIVER_NUM, 9, 0, 0).to_result()?;
		    if packet_recv != 0 {
			// Contains a 1 in the most significant bit to indicate
			// that a (possibly empty) packet was received.
			// writeln!(Console::writer(), "packet_recv & 0xFFFF = {}", packet_recv as u16).unwrap();
			return Ok(Some(packet_recv as u16));
		    }

                    if block {
			loop {
                            S::yield_wait();
                            if let Some((packet_len,)) = called.get() {
				// writeln!(Console::writer(), "callback delivered: {}", packet_len as u16).unwrap();
                                return Ok(Some(packet_len as u16));
                            }
                        }
                    } else {
			// writeln!(Console::writer(), "=> Ok(None)").unwrap();
			Ok(None)
		    }
                },
            )?;

        if let Some(len) = packet_received {
	    S::command(DRIVER_NUM, 7, 0, 0).to_result::<(), ErrorCode>()?;
            self.rx_packet.set(Some(len));
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl<'a, const ETH_MTU: usize, S: Syscalls + 'a, C: Config + 'a> smoltcp::phy::Device<'a>
    for TockTapDevice<ETH_MTU, S, C>
{
    type RxToken = TockTapRxToken<'a, ETH_MTU>;
    type TxToken = TockTapTxToken<'a, ETH_MTU, S, C>;

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.try_receive(false).unwrap();

        if let Some(len) = self.rx_packet.get() {
	    writeln!(Console::writer(), "Received packet of len {}", len).unwrap();
	    self.rx_packet.set(None);
            Some((
                TockTapRxToken(&mut self.rx_buffer),
                TockTapTxToken(&mut self.tx_buffer, PhantomData),
            ))
        } else {
            None
        }
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(TockTapTxToken(&mut self.tx_buffer, PhantomData))
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut checksum_cap = smoltcp::phy::ChecksumCapabilities::ignored();
        checksum_cap.ipv4 = smoltcp::phy::Checksum::Both;
        checksum_cap.udp = smoltcp::phy::Checksum::Both;
        checksum_cap.tcp = smoltcp::phy::Checksum::Both;
        checksum_cap.icmpv4 = smoltcp::phy::Checksum::Both;
        checksum_cap.icmpv6 = smoltcp::phy::Checksum::Both;

        let mut dev_cap = smoltcp::phy::DeviceCapabilities::default();
        dev_cap.medium = smoltcp::phy::Medium::Ethernet;
        dev_cap.max_transmission_unit = ETH_MTU;
        dev_cap.checksum = checksum_cap;
        dev_cap
    }
}

fn smoltcp_instant() -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_millis(
	Alarm::current_instant::<Milliseconds>().unwrap().0)
}

fn main() {
    let device: TockTapDevice<1520, TockSyscalls> = TockTapDevice::new();

    let mut neighbor_cache_storage = [None; 8];
    let neighbor_cache = smoltcp::iface::NeighborCache::new(&mut neighbor_cache_storage[..]);

    let ethernet_addr = smoltcp::wire::EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mut ip_addrs = [smoltcp::wire::IpCidr::new(
        smoltcp::wire::IpAddress::v4(192, 168, 1, 50),
        24,
    )];

    let mut udp_rx_pkt_metadata = [smoltcp::storage::PacketMetadata::EMPTY; 2];
    let mut udp_rx_pkt_buffer = [0; 1520];
    let udp_rx_buffer = smoltcp::socket::UdpSocketBuffer::new(
        &mut udp_rx_pkt_metadata[..],
        &mut udp_rx_pkt_buffer[..],
    );

    let mut udp_tx_pkt_metadata = [smoltcp::storage::PacketMetadata::EMPTY; 2];
    let mut udp_tx_pkt_buffer = [0; 1520];
    let udp_tx_buffer = smoltcp::socket::UdpSocketBuffer::new(
        &mut udp_tx_pkt_metadata[..],
        &mut udp_tx_pkt_buffer[..],
    );

    let udp_socket = smoltcp::socket::UdpSocket::new(udp_rx_buffer, udp_tx_buffer);

    let mut tcp_rx_buffer_arr = [0; 64];
    let tcp_rx_buffer = smoltcp::socket::TcpSocketBuffer::new(&mut tcp_rx_buffer_arr[..]);
    let mut tcp_tx_buffer_arr = [0; 128];
    let tcp_tx_buffer = smoltcp::socket::TcpSocketBuffer::new(&mut tcp_tx_buffer_arr[..]);
    let tcp_socket = smoltcp::socket::TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);

    let mut sockets = [smoltcp::iface::SocketStorage::EMPTY; 2];
    let mut iface = smoltcp::iface::InterfaceBuilder::new(device, &mut sockets[..])
        .ip_addrs(&mut ip_addrs[..])
        .hardware_addr(ethernet_addr.into())
        .neighbor_cache(neighbor_cache)
        .finalize();

    let udp_handle = iface.add_socket(udp_socket);
    let tcp_handle = iface.add_socket(tcp_socket);

    loop {
	let mut timestamp = smoltcp_instant();
	match iface.poll(timestamp) {
	    Ok(_) => {},
	    Err(e) => {
		writeln!(Console::writer(), "Poll error: {:?}", e).unwrap();
	    },
	}

	// ---------- UDP SOCKET -----------------------------------------------

	let socket = iface.get_socket::<smoltcp::socket::UdpSocket>(udp_handle);
	if !socket.is_open() {
	    socket.bind(6969).unwrap();
	}

	let client = match socket.recv() {
	    Ok((data, endpoint)) => {
		writeln!(Console::writer(), "udp:6969 recv {} bytes from {}", data.len(), endpoint).unwrap();
		Some(endpoint)
	    },
	    Err(_) => None,
	};

	if let Some(endpoint) = client {
	    let data = b"hello\n";
	    socket.send_slice(data, endpoint).unwrap();
	}
	
	// ---------- TCP SOCKET -----------------------------------------------

	let socket = iface.get_socket::<smoltcp::socket::TcpSocket>(tcp_handle);
        if !socket.is_open() {
            socket.listen(6969).unwrap();
	    socket.set_ack_delay(None);
	    socket.set_nagle_enabled(false);
        }

	// if socket.may_recv() {
        //     socket
        //         .recv(|buffer| {
        //             if !buffer.is_empty() {
        //                 writeln!(Console::writer(), "tcp:6969 recv {:?} octets", buffer.len()).unwrap();
        //             }
        //             (buffer.len(), ())
        //         })
        //         .unwrap();
	// }

        if socket.can_send() {
	    // writeln!(Console::writer(), "tcp:6969 open, sending...").unwrap();
            // writeln!(socket, "hello").unwrap();
            // socket.close();
        }

	let mut next_poll_time = iface.poll_at(timestamp).unwrap_or(timestamp);
	while {
	    timestamp = smoltcp_instant();
	    !iface.device_mut().try_receive(false).unwrap() && timestamp <= next_poll_time
	} {
	    next_poll_time = iface.poll_at(timestamp).unwrap_or(timestamp);
	    Alarm::sleep_for(Milliseconds(1)).unwrap();
	}
    }
}
