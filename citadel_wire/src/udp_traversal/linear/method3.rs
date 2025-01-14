use std::collections::HashMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::Duration;

use crate::error::FirewallError;
use crate::socket_helpers::ensure_ipv6;
use crate::udp_traversal::linear::encrypted_config_container::EncryptedConfigContainer;
use crate::udp_traversal::targetted_udp_socket_addr::TargettedSocketAddr;
use crate::udp_traversal::HolePunchID;
use citadel_runtime::Mutex;
use netbeam::sync::RelativeNodeType;
use std::sync::Arc;

/// Method three: "Both sides send packets with short TTL values followed by packets with long TTL
// values". Source: page 7 of https://thomaspbraun.com/pdfs/NAT_Traversal/NAT_Traversal.pdf
pub struct Method3 {
    this_node_type: RelativeNodeType,
    encrypted_config: EncryptedConfigContainer,
    unique_id: HolePunchID,
    // in the case the adjacent node for id=key succeeds, yet, this node fails, recovery mode can ensue
    observed_addrs_on_syn: Mutex<HashMap<HolePunchID, TargettedSocketAddr>>,
}

#[derive(Serialize, Deserialize)]
enum NatPacket {
    Syn(HolePunchID, u32, RelativeNodeType),
    // contains the local bind addr of candidate for socket identification
    SynAck(HolePunchID, RelativeNodeType),
}

impl Method3 {
    /// Make sure to complete the pre-process stage before calling this
    pub fn new(
        this_node_type: RelativeNodeType,
        encrypted_config: EncryptedConfigContainer,
        unique_id: HolePunchID,
    ) -> Self {
        Self {
            this_node_type,
            encrypted_config,
            unique_id,
            observed_addrs_on_syn: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn execute(
        &self,
        socket: &UdpSocket,
        endpoints: &Vec<SocketAddr>,
    ) -> Result<TargettedSocketAddr, FirewallError> {
        match self.this_node_type {
            RelativeNodeType::Initiator => self.execute_either(socket, endpoints).await,

            RelativeNodeType::Receiver => self.execute_either(socket, endpoints).await,
        }
    }

    pub(crate) fn get_peer_external_addr_from_peer_hole_punch_id(
        &self,
        id: HolePunchID,
    ) -> Option<TargettedSocketAddr> {
        let lock = self.observed_addrs_on_syn.lock();
        log::trace!(target: "citadel", "Recv'd SYNS: {:?}", &*lock);
        lock.get(&id).copied()
    }

    /// The initiator must pass a vector correlating to the target endpoints. Each provided socket will attempt to reach out to the target endpoint (1-1)
    ///
    /// Note! The endpoints should be the port-predicted addrs
    #[cfg_attr(
        feature = "localhost-testing",
        tracing::instrument(target = "citadel", skip_all, ret, err(Debug))
    )]
    async fn execute_either(
        &self,
        socket: &UdpSocket,
        endpoints: &Vec<SocketAddr>,
    ) -> Result<TargettedSocketAddr, FirewallError> {
        let default_ttl = socket.ttl().ok();
        log::trace!(target: "citadel", "Default TTL: {:?}", default_ttl);
        let unique_id = &self.unique_id.clone();
        let this_node_type = self.this_node_type;
        // We will begin sending packets right away, assuming the pre-process synchronization occurred
        // 400ms window
        let encryptor = &self.encrypted_config;
        let observed_addrs_on_syn = &self.observed_addrs_on_syn;

        let socket_wrapper = &UdpWrapper::new(socket);

        const MILLIS_DELTA: u64 = 20;

        let receiver_task = async move {
            // we are only interested in the first receiver to receive a value
            if let Ok(res) = tokio::time::timeout(
                Duration::from_millis(2000),
                Self::recv_until(
                    socket_wrapper,
                    encryptor,
                    unique_id,
                    observed_addrs_on_syn,
                    MILLIS_DELTA,
                    this_node_type,
                ),
            )
            .await
            .map_err(|err| FirewallError::HolePunch(err.to_string()))?
            {
                Ok(res)
            } else {
                Err(FirewallError::HolePunch(
                    "No UDP penetration detected".to_string(),
                ))
            }
        };

        let sender_task = async move {
            //tokio::time::sleep(Duration::from_millis(10)).await; // wait to allow time for the joined receiver task to execute
            Self::send_syn_barrage(
                20,
                Some(60),
                socket_wrapper,
                endpoints,
                encryptor,
                MILLIS_DELTA,
                2,
                *unique_id,
                this_node_type,
            )
            .await
            .map_err(|err| FirewallError::HolePunch(err.to_string()))?;
            //Self::send_syn_barrage(120, None, socket_wrapper, endpoints, encryptor,  MILLIS_DELTA, 3,unique_id.clone()).await.map_err(|err| FirewallError::HolePunch(err.to_string()))?;
            Ok(()) as Result<(), FirewallError>
        };

        let (res0, res1) = tokio::join!(receiver_task, sender_task);

        log::trace!(target: "citadel", "Hole-punch join result: recv={:?} and send={:?}", res0, res1);

        if let Some(default_ttl) = default_ttl {
            socket
                .set_ttl(default_ttl)
                .map_err(|err| FirewallError::HolePunch(err.to_string()))?;
        }

        let hole_punched_addr = res0?;

        log::trace!(target: "citadel", "Completed hole-punch...");

        Ok(hole_punched_addr)
    }

    /// Some research papers explain that incrementing the TTL on the packet may be beneficial
    #[allow(clippy::too_many_arguments)]
    async fn send_syn_barrage(
        ttl_init: u32,
        delta_ttl: Option<u32>,
        socket: &UdpWrapper<'_>,
        endpoints: &Vec<SocketAddr>,
        encryptor: &EncryptedConfigContainer,
        millis_delta: u64,
        count: u32,
        unique_id: HolePunchID,
        this_node_type: RelativeNodeType,
    ) -> Result<(), anyhow::Error> {
        let mut sleep = tokio::time::interval(Duration::from_millis(millis_delta));
        let delta_ttl = delta_ttl.unwrap_or(0);
        let ttls = (0..count)
            .into_iter()
            .map(|idx| ttl_init + (idx * delta_ttl))
            .collect::<Vec<u32>>();

        // fan-out all packets from a singular source to multiple consumers using the ttls specified
        for ttl in ttls {
            let _ = sleep.tick().await;
            for endpoint in endpoints {
                let packet = encryptor.generate_packet(
                    &bincode2::serialize(&NatPacket::Syn(unique_id, ttl, this_node_type)).unwrap(),
                );
                log::trace!(target: "citadel", "Sending TTL={} to {} || {:?}", ttl, endpoint, &packet[..] as &[u8]);
                if !socket.send(&packet, *endpoint, Some(ttl)).await? {
                    log::trace!(target: "citadel", "Early-terminating SYN barrage");
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    // Handles the reception of packets, as well as sending/awaiting for a verification
    async fn recv_until(
        socket: &UdpWrapper<'_>,
        encryptor: &EncryptedConfigContainer,
        unique_id: &HolePunchID,
        observed_addrs_on_syn: &Mutex<HashMap<HolePunchID, TargettedSocketAddr>>,
        millis_delta: u64,
        this_node_type: RelativeNodeType,
    ) -> Result<TargettedSocketAddr, FirewallError> {
        let buf = &mut [0u8; 4096];
        log::trace!(target: "citadel", "[Hole-punch] Listening on {:?}", socket.socket.local_addr().unwrap());

        let mut has_received_syn = false;
        let mut expected_response_addr = None;
        //let mut recv_from_required = None;
        while let Ok((len, peer_external_addr)) = socket.recv_from(buf).await {
            log::trace!(target: "citadel", "[UDP Hole-punch] RECV packet from {:?} | {:?}", &peer_external_addr, &buf[..len]);
            let packet = match encryptor.decrypt_packet(&buf[..len]) {
                Some(plaintext) => plaintext,
                _ => {
                    log::warn!(target: "citadel", "BAD Hole-punch packet: decryption failed!");
                    continue;
                }
            };

            match bincode2::deserialize(&packet)
                .map_err(|err| FirewallError::HolePunch(err.to_string()))
            {
                Ok(NatPacket::Syn(peer_unique_id, ttl, adjacent_node_type)) => {
                    if adjacent_node_type == this_node_type {
                        log::warn!(target: "citadel", "RECV loopback packet; will discard");
                        continue;
                    }

                    if has_received_syn {
                        continue;
                    }

                    log::trace!(target: "citadel", "RECV SYN from {:?}", peer_unique_id);
                    let hole_punched_addr = TargettedSocketAddr::new(
                        peer_external_addr,
                        peer_external_addr,
                        peer_unique_id,
                    );

                    observed_addrs_on_syn
                        .lock()
                        .insert(peer_unique_id, hole_punched_addr);

                    let mut sleep = tokio::time::interval(Duration::from_millis(millis_delta));

                    log::trace!(target: "citadel", "Received TTL={} packet from {:?}. Awaiting mutual recognition... ", ttl, peer_external_addr);

                    has_received_syn = true;
                    expected_response_addr = Some(peer_external_addr);

                    for ttl in [4, 60, 120] {
                        sleep.tick().await;
                        let packet = &encryptor.generate_packet(
                            &bincode2::serialize(&NatPacket::SynAck(*unique_id, this_node_type))
                                .unwrap(),
                        );
                        log::trace!(target: "citadel", "[Syn->SynAck] Sending TTL={} to {} || {:?}", ttl, peer_external_addr, &packet[..] as &[u8]);
                        socket.send(packet, peer_external_addr, Some(ttl)).await?;
                    }
                }

                // the reception of a SynAck proves the existence of a hole punched since there is bidirectional communication through the NAT
                Ok(NatPacket::SynAck(adjacent_unique_id, adjacent_node_type)) => {
                    log::trace!(target: "citadel", "RECV SYN_ACK");
                    if adjacent_node_type == this_node_type {
                        log::warn!(target: "citadel", "RECV hairpin packet; will discard");
                        continue;
                    }

                    if !has_received_syn {
                        log::warn!(target: "citadel", "RECV early SYN_ACK. Will discard");
                        continue;
                    }

                    let expected_addr = expected_response_addr.unwrap();

                    if peer_external_addr != expected_addr {
                        log::warn!(target: "citadel", "[will allow] RECV SYN_ACK that comes from the wrong addr. RECV: {:?}, Expected: {:?}", peer_external_addr, expected_addr);
                        //continue;
                    }

                    // this means there was a successful ping-pong. We can now assume this communications line is valid since the nat addrs match
                    let hole_punched_addr = TargettedSocketAddr::new(
                        peer_external_addr,
                        peer_external_addr,
                        adjacent_unique_id,
                    );
                    log::trace!(target: "citadel", "***UDP Hole-punch to {:?} success!***", &hole_punched_addr);
                    socket.stop_outgoing_traffic().await;

                    return Ok(hole_punched_addr);
                }

                Err(err) => {
                    log::warn!(target: "citadel", "Unable to deserialize packet {:?} from {:?}: {:?}", &packet[..], peer_external_addr, err);
                }
            }
        }

        Err(FirewallError::HolePunch("Socket recv error".to_string()))
    }
}

/// Used to enforce mutual exclusion writing
struct UdpWrapper<'a> {
    lock: Arc<TokioMutex<bool>>,
    socket: &'a UdpSocket,
}

impl UdpWrapper<'_> {
    fn new(socket: &UdpSocket) -> UdpWrapper {
        UdpWrapper {
            lock: Arc::new(TokioMutex::new(true)),
            socket,
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    /// Returns false if
    async fn send(
        &self,
        buf: &[u8],
        mut to: SocketAddr,
        ttl: Option<u32>,
    ) -> std::io::Result<bool> {
        let lock = self.lock.lock().await;
        if !*lock {
            return Ok(false);
        }

        if let Some(ttl) = ttl {
            let _ = self.socket.set_ttl(ttl);
        }

        let local_addr = self.socket.local_addr()?;

        if to.is_ipv6() && local_addr.is_ipv4() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Send_to is ipv6, but bind is ipv4",
            ));
        }

        if local_addr.is_ipv6() {
            to = SocketAddr::V6(ensure_ipv6(to))
        }

        self.socket.send_to(buf, to).await?;
        Ok(true)
    }

    async fn stop_outgoing_traffic(&self) {
        *self.lock.lock().await = false
    }
}
