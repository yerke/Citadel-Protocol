use std::net::{SocketAddr, IpAddr};
use std::fmt::{Display, Formatter};
use tokio::net::UdpSocket;
use serde::{Serialize, Deserialize};
use crate::udp_traversal::HolePunchID;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TargettedSocketAddr {
    // Outbound packets should get sent here
    pub initial: SocketAddr,
    pub natted: SocketAddr,
    pub unique_id: HolePunchID
}

impl TargettedSocketAddr {
    pub fn new(initial: SocketAddr, natted: SocketAddr, unique_id: HolePunchID) -> Self {
        Self { initial, natted, unique_id }
    }

    pub fn new_invariant(addr: SocketAddr) -> Self {
        Self { initial: addr, natted: addr, unique_id: HolePunchID::new() }
    }

    pub fn ip_translated(&self) -> bool {
        self.initial.ip() != self.natted.ip()
    }

    pub fn port_translated(&self) -> bool {
        self.initial.port() != self.natted.port()
    }

    pub fn eq_to(&self, ip_addr: IpAddr, port: u16) -> bool {
        (ip_addr == self.initial.ip() && port == self.initial.port()) ||
            (ip_addr == self.natted.ip() && port == self.natted.port())
    }

    pub fn recv_packet_valid(&self, recv_packet_socket: SocketAddr) -> bool {
        recv_packet_socket.ip() == self.natted.ip()
    }
}

impl Display for TargettedSocketAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "(Original, Natted): {:?} -> {:?}", &self.initial, &self.natted)
    }
}

#[derive(Debug)]
pub struct HolePunchedUdpSocket {
    pub socket: UdpSocket,
    pub addr: TargettedSocketAddr
}

impl HolePunchedUdpSocket {
    // After hole-punching, some packets may be sent that need to be flushed
    // this cleanses the stream
    pub(crate) fn cleanse(&self) -> std::io::Result<()> {
        let buf = &mut [0u8; 1024];
        loop {
            match self.socket.try_recv(buf) {
                Ok(_) => {
                    continue;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(())
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }
}