use crate::hdp::hdp_node::Ticket;
use crate::hdp::packet_processor::includes::Instant;
use crate::hdp::peer::channel::UdpChannel;
use hyxe_crypt::stacked_ratchet::constructor::StackedRatchetConstructor;
use hyxe_crypt::stacked_ratchet::StackedRatchet;
use hyxe_wire::hypernode_type::NodeType;
use tokio::net::UdpSocket;
use tokio::sync::oneshot::{channel, Receiver, Sender};

/// For keeping track of the pre-connect state
pub struct PreConnectState {
    pub(crate) last_stage: u8,
    #[allow(dead_code)]
    pub(crate) adjacent_node_type: Option<NodeType>,
    // This drill should be turned .into() the next toolset once the other side updated
    pub(crate) constructor: Option<StackedRatchetConstructor>,
    pub(crate) ticket: Option<Ticket>,
    pub(crate) last_packet_time: Option<Instant>,
    pub(crate) udp_channel_oneshot_tx: UdpChannelSender,
    pub(crate) success: bool,
    pub(crate) unused_local_udp_socket: Option<UdpSocket>,
    pub(crate) generated_ratchet: Option<StackedRatchet>,
}

impl PreConnectState {
    pub fn on_packet_received(&mut self) {
        self.last_packet_time = Some(Instant::now());
    }
}

impl Default for PreConnectState {
    fn default() -> Self {
        Self {
            generated_ratchet: None,
            udp_channel_oneshot_tx: UdpChannelSender::empty(),
            constructor: None,
            last_packet_time: None,
            last_stage: 0,
            adjacent_node_type: None,
            success: false,
            ticket: None,
            unused_local_udp_socket: None,
        }
    }
}

pub struct UdpChannelSender {
    pub tx: Option<Sender<UdpChannel>>,
    pub rx: Option<Receiver<UdpChannel>>,
}

impl UdpChannelSender {
    pub(crate) fn empty() -> Self {
        Self { tx: None, rx: None }
    }
}

impl Default for UdpChannelSender {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx: Some(tx),
            rx: Some(rx),
        }
    }
}
