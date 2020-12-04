use crate::hdp::hdp_server::{HdpServerRemote, Ticket, HdpServerRequest};
use hyxe_crypt::drill::SecurityLevel;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::error::NetworkError;
use crate::hdp::state_container::VirtualConnectionType;
use futures::channel::mpsc::UnboundedReceiver;
use futures::{Sink, Stream};
use futures::task::{Context, Poll, Waker};
use tokio::macros::support::Pin;
use std::fmt::Debug;
use crate::hdp::peer::peer_layer::PeerConnectionType;
use tokio::io::AsyncWrite;
use futures::io::Error;
use std::ops::Deref;
use hyxe_crypt::sec_bytes::SecBuffer;

// 1 peer channel per virtual connection. This enables high-level communication between the [HdpServer] and the API-layer.
// This thus bypasses the kernel.
#[derive(Debug)]
pub struct PeerChannel {
    send_half: PeerChannelSendHalf,
    recv_half: PeerChannelRecvHalf
}

impl PeerChannel {
    pub(crate) fn new(server_remote: HdpServerRemote, target_cid: u64, vconn_type: VirtualConnectionType, channel_id: Ticket, security_level: SecurityLevel, is_alive: Arc<AtomicBool>, receiver: UnboundedReceiver<SecBuffer>) -> (Self, tokio::sync::oneshot::Receiver<Waker>) {
        let (waker_send, waker_recv) = tokio::sync::oneshot::channel();
        let implicated_cid = vconn_type.get_implicated_cid();
        let send_half = PeerChannelSendHalf {
            server_remote,
            target_cid,
            vconn_type,
            implicated_cid,
            channel_id,
            security_level,
            is_alive: is_alive.clone()
        };

        let recv_half = PeerChannelRecvHalf {
            receiver,
            target_cid,
            vconn_type,
            channel_id,
            is_alive,
            waker_send: Some(waker_send)
        };

        (PeerChannel { send_half, recv_half }, waker_recv)
    }

    /// Gets the CID of the endpoint
    pub fn get_peer_cid(&self) -> u64 {
        self.send_half.target_cid
    }

    /// Gets the CID of the local user
    pub fn get_implicated_cid(&self) -> u64 {
        self.send_half.vconn_type.get_implicated_cid()
    }

    /// Gets the metadata of the virtual connection
    pub fn get_peer_conn_type(&self) -> Option<PeerConnectionType> {
        match self.send_half.vconn_type {
            VirtualConnectionType::HyperLANPeerToHyperLANPeer(implicated_cid, target_cid) => {
                Some(PeerConnectionType::HyperLANPeerToHyperLANPeer(implicated_cid, target_cid))
            }

            VirtualConnectionType::HyperLANPeerToHyperWANPeer(implicated_cid, icid, target_cid) => {
                Some(PeerConnectionType::HyperLANPeerToHyperWANPeer(implicated_cid, icid, target_cid))
            }

            _ => None
        }
    }

    /// In order to use the [PeerChannel] properly, split must be called in order to receive
    /// an asynchronous interface. The SendHalf implements Sink, whereas the RecvHalf implements
    /// Stream
    pub fn split(self) -> (PeerChannelSendHalf, PeerChannelRecvHalf) {
        (self.send_half, self.recv_half)
    }
}

#[derive(Debug, Clone)]
pub struct PeerChannelSendHalf {
    server_remote: HdpServerRemote,
    target_cid: u64,
    implicated_cid: u64,
    vconn_type: VirtualConnectionType,
    channel_id: Ticket,
    security_level: SecurityLevel,
    // When the associated virtual conn drops, this gets flipped off, and hence, data won't be sent anymore
    is_alive: Arc<AtomicBool>
}

impl PeerChannelSendHalf {
    fn send_unchecked(&self, data: SecBuffer) -> Result<(), NetworkError> {
        let request = HdpServerRequest::SendMessage(data, self.implicated_cid, self.vconn_type, self.security_level);
        self.server_remote.send_with_custom_ticket(self.channel_id, request);
        Ok(())
    }

    pub fn send_unbounded(&self, data: SecBuffer) -> Result<(), NetworkError> {
        if self.is_alive.load(Ordering::SeqCst) {
            self.send_unchecked(data)
        } else {
            Err(NetworkError::InternalError("Server closed"))
        }
    }

    pub fn set_security_level(&mut self, security_level: SecurityLevel) {
        self.security_level = security_level;
    }
}

impl Sink<SecBuffer> for PeerChannelSendHalf {
    type Error = NetworkError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.is_alive.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(NetworkError::InternalError("Server closed")))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: SecBuffer) -> Result<(), Self::Error> {
        self.send_unchecked(item)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl Unpin for PeerChannelRecvHalf {}

#[derive(Debug)]
pub struct PeerChannelRecvHalf {
    // when the state container removes the vconn, this will get closed
    receiver: UnboundedReceiver<SecBuffer>,
    target_cid: u64,
    vconn_type: VirtualConnectionType,
    channel_id: Ticket,
    is_alive: Arc<AtomicBool>,
    waker_send: Option<tokio::sync::oneshot::Sender<Waker>>
}

impl Stream for PeerChannelRecvHalf {
    type Item = SecBuffer;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.is_alive.load(Ordering::SeqCst) {
            // close the stream
            log::info!("[POLL] closing PeerChannel RecvHalf");
            Poll::Ready(None)
        } else {
            // on first poll, we need to send the waker. No messages will be queued though
            if let Some(waker_send) = self.waker_send.take() {
                if let Err(_) = waker_send.send(cx.waker().clone()) {
                    log::error!("[Channel] SendHalf has dropped");
                    self.is_alive.store(false, Ordering::SeqCst);
                    return Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            } else {
                match self.receiver.try_next() {
                    Ok(Some(data)) => Poll::Ready(Some(data)),
                    Err(_) => {
                        // when the stream yields Some, it will get polled again.
                        // when that occurs, try_next likely returns None and
                        // we need to signal for Pending to be awoken again
                        Poll::Pending
                    }
                    _ => Poll::Pending
                }
            }
        }
    }
}

unsafe impl Send for PeerChannelSendHalf {}
unsafe impl Send for PeerChannelRecvHalf {}
unsafe impl Send for PeerChannel {}

impl AsyncWrite for PeerChannelSendHalf {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        self.deref().send_unbounded(SecBuffer::from(buf))
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err.to_string()))?;

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        futures::Sink::poll_flush(self, cx)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err.to_string()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.is_alive.store(false, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}