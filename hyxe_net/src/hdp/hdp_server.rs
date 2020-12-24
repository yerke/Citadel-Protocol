use std::fmt::{Display, Formatter, Debug};
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use nanoserde::{SerBin, DeBin};

use std::net::SocketAddr;
use futures::{StreamExt, Sink};
use crate::hdp::outbound_sender::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::try_join;
use log::info;
use tokio::net::{TcpListener, TcpStream};
use std::net::ToSocketAddrs;

use hyxe_crypt::drill::SecurityLevel;
use hyxe_user::account_manager::AccountManager;
use hyxe_user::client_account::ClientNetworkAccount;

use crate::error::NetworkError;
use crate::hdp::hdp_session_manager::HdpSessionManager;
use crate::hdp::state_container::{VirtualConnectionType, VirtualTargetType, FileKey};
use crate::proposed_credentials::ProposedCredentials;
use hyxe_nat::local_firewall_handler::{open_local_firewall_port, remove_firewall_rule, FirewallProtocol};
use hyxe_nat::hypernode_type::HyperNodeType;
use hyxe_nat::time_tracker::TimeTracker;
use crate::hdp::peer::peer_layer::{PeerSignal, MailboxTransfer};
use std::task::{Context, Poll};
use std::pin::Pin;
use crate::hdp::peer::channel::PeerChannel;
use std::path::PathBuf;
use crate::hdp::hdp_packet_processor::includes::{Instant, Duration};
use crate::constants::{NTP_RESYNC_FREQUENCY, TCP_CONN_TIMEOUT};
use crate::hdp::hdp_packet_processor::peer::group_broadcast::GroupBroadcast;
use crate::kernel::runtime_handler::RuntimeHandler;
use crate::hdp::file_transfer::FileTransferStatus;
use hyxe_crypt::sec_bytes::SecBuffer;
use parking_lot::Mutex;

/// ports which were opened that must be closed atexit
static OPENED_PORTS: Mutex<Vec<u16>> = parking_lot::const_mutex(Vec::new());

pub extern fn atexit() {
    log::info!("Cleaning up firewall ports ...");
    let lock = OPENED_PORTS.lock();
    for port in lock.iter() {
        HdpServer::close_tcp_port(*port);
    }
}

// The outermost abstraction for the networking layer. We use Rc to allow ensure single-threaded performance
// by default, but settings can be changed in crate::macros::*.
define_outer_struct_wrapper!(HdpServer, HdpServerInner);

/// Inner device for the HdpServer
pub struct HdpServerInner {
    primary_socket: Option<TcpListener>,
    /// Key: cid (to account for multiple clients from the same node)
    session_manager: HdpSessionManager,
    local_bind_addr: SocketAddr,
    system_engaged: bool,
    to_kernel: UnboundedSender<HdpServerResult>,
    local_node_type: HyperNodeType,
    shutdown_signaller: Option<tokio::sync::oneshot::Sender<()>>
}

impl HdpServer {
    /// Creates a new [HdpServer]
    pub async fn new<T: tokio::net::ToSocketAddrs + std::net::ToSocketAddrs>(local_node_type: HyperNodeType, to_kernel: UnboundedSender<HdpServerResult>, bind_addr: T, account_manager: AccountManager) -> io::Result<Self> {
        let (primary_socket, local_bind_addr) = Self::create_tcp_listen_socket(&bind_addr)?;
        let primary_port = local_bind_addr.port();
        // Note: on Android/IOS, the below command will fail since sudo access is prohibited
        Self::open_tcp_port(primary_port);

        info!("Server established on {}", local_bind_addr);

        let time_tracker = TimeTracker::new().await?;
        let session_manager = HdpSessionManager::new(local_node_type,to_kernel.clone(), account_manager, time_tracker.clone(), false);
        let inner = HdpServerInner {
            shutdown_signaller: None,
            local_bind_addr,
            local_node_type,
            primary_socket: Some(primary_socket),
            to_kernel,
            session_manager,
            system_engaged: false,
        };

        Ok(Self::from(inner))
    }

    /// Note: spawning via handle is more efficient than joining futures. Source: https://cafbit.com/post/tokio_internals/
    /// To handle the shutdown process, we need
    ///
    /// This will panic if called twice in succession without a proper server reload.
    ///
    /// Returns a handle to communicate with the [HdpServer].
    /// TODO: if local is pure_server mode, bind the local sockets
    #[allow(unused_results)]
    pub async fn load(this: HdpServer, handle: &RuntimeHandler) -> Result<HdpServerRemote, NetworkError> {
        // Allow the listeners to read data without instantly returning
        // Load the readers
        let mut write = inner_mut!(this);

        write.system_engaged = true;
        let kernel_tx = write.to_kernel.clone();
        let (shutdown_signaller, shutdown_receiver) = tokio::sync::oneshot::channel();
        write.shutdown_signaller = Some(shutdown_signaller);


        let (outbound_send_request_tx, outbound_send_request_rx) = unbounded(); // for the Hdp remote
        // Load the writer
        load_into_runtime!(handle, Self::outbound_kernel_request_handler(this.clone(), kernel_tx.clone(), outbound_send_request_rx));
        let remote = HdpServerRemote::new(outbound_send_request_tx);
        let tt = write.session_manager.load_server_remote_get_tt(remote.clone());
        load_into_runtime!(handle, Self::listen_primary(this.clone(), tt, shutdown_receiver, kernel_tx.clone()));

        load_into_runtime!(handle, HdpSessionManager::run_peer_container(write.session_manager.clone()));

        Ok(remote)
    }

    fn open_tcp_port(port: u16) {
        if let Ok(res) = open_local_firewall_port(FirewallProtocol::TCP(port)) {
            if !res.status.success() {
                let data = if res.stdout.is_empty() { res.stderr } else { res.stdout };
                log::warn!("We were unable to ensure that port {}, be open. Reason: {}", port, String::from_utf8(data).unwrap_or_default());
            } else {
                OPENED_PORTS.lock().push(port);
            }
        }
    }

    fn close_tcp_port(port: u16) {
        if let Ok(res) = remove_firewall_rule(FirewallProtocol::TCP(port)) {
            if !res.status.success() {
                let data = if res.stdout.is_empty() { res.stderr } else { res.stdout };
                log::warn!("We were unable to ensure that port {}, be CLOSED. Reason: {}", port, String::from_utf8(data).unwrap_or_default());
            } else {
                log::info!("Successfully shutdown port {}", port);
            }
        }
    }

    pub(crate) fn create_tcp_listen_socket<T: ToSocketAddrs>(full_bind_addr: T) -> io::Result<(TcpListener, SocketAddr)> {
        let bind: SocketAddr = full_bind_addr.to_socket_addrs()?.next().ok_or(std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "bad addr"))?;
        if bind.is_ipv4() {
            let ref builder = net2::TcpBuilder::new_v4()?;
            Self::bind_defaults(builder, bind, 1024)
        } else {
            let builder = net2::TcpBuilder::new_v6()?;
            Self::bind_defaults(builder.only_v6(false)?, bind, 1024)
        }
    }

    fn bind_defaults(builder: &net2::TcpBuilder, bind: SocketAddr, backlog: i32) -> io::Result<(TcpListener, SocketAddr)> {
        builder
            .reuse_address(true)?
            .bind(bind)?
            .listen(backlog) //default
            .map(tokio::net::TcpListener::from_std)?
            .and_then(|listener| {
                Ok((listener, bind))
            })
    }

    /// Returns a TcpStream to the remote addr, as well as a local TcpListener on the same bind addr going to remote
    /// to allow for TCP hole-punching (we need the same port to cover port-restricted NATS, worst-case scenario)
    pub(crate) async fn create_init_tcp_connect_socket<R: ToSocketAddrs>(remote: R) -> io::Result<(TcpListener, TcpStream)> {
        let stream = Self::create_reuse_tcp_connect_socket(remote, None).await?;

        let stream_bind_addr = stream.local_addr()?;

        let (p2p_listener, _stream_bind_addr) = if stream_bind_addr.is_ipv4() {
            let ref builder = net2::TcpBuilder::new_v4()?;
            Self::bind_defaults(builder, stream_bind_addr, 16)?
        } else {
            let builder = net2::TcpBuilder::new_v6()?;
            Self::bind_defaults(builder.only_v6(false)?, stream_bind_addr, 16)?
        };
        Self::open_tcp_port(stream_bind_addr.port());

        Ok((p2p_listener, stream))
    }

    pub(crate) async fn create_reuse_tcp_connect_socket<R: ToSocketAddrs>(remote: R, timeout: Option<Duration>) -> io::Result<TcpStream> {
        let remote: SocketAddr = remote.to_socket_addrs()?.next().ok_or(std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "bad addr"))?;
        Self::connect_defaults(timeout, remote).await
    }

    async fn connect_defaults(timeout: Option<Duration>, remote: SocketAddr) -> io::Result<tokio::net::TcpStream> {
        tokio::time::timeout(timeout.unwrap_or(TCP_CONN_TIMEOUT), tokio::task::spawn_blocking(move || {
            let std_stream = if remote.is_ipv4() {
                net2::TcpBuilder::new_v4()?
                    .reuse_address(true)?
                    .connect(remote)?
            } else {
                net2::TcpBuilder::new_v6()?
                    .only_v6(false)?
                    .reuse_address(true)?
                    .connect(remote)?
            };

            let stream = tokio::net::TcpStream::from_std(std_stream)?;
            stream.set_linger(Some(tokio::time::Duration::from_secs(0)))?;
            stream.set_keepalive(None)?;

            Ok(stream)
        })).await??
    }

    /// In impersonal mode, each hypernode needs to check for incoming connections on the primary port.
    /// Once a TcpStream is established, it is passed into the underlying HdpSessionManager and a Session
    /// is created to handle the stream.
    ///
    /// In personal mode, if a new connection needs to be forged with another node, then a new SO_REUSE socket
    /// will need to be created that is bound to the local primary port and connected to the adjacent hypernode's
    /// primary port. That socket will be created in the underlying HdpSessionManager during the connection process
    async fn listen_primary(server: HdpServer, tt: TimeTracker, shutdown_receiver: tokio::sync::oneshot::Receiver<()>, to_kernel: UnboundedSender<HdpServerResult>) -> Result<(), NetworkError> {
        let mut this = inner_mut!(server);
        let socket = this.primary_socket.take().unwrap();
        let session_manager = this.session_manager.clone();
        std::mem::drop(this);
        let timer_future = Self::primary_timer(shutdown_receiver, to_kernel.clone());
        let primary_port_future = Self::primary_session_creator_loop(to_kernel, session_manager, socket);
        let tt_updater_future = Self::time_tracker_updater(tt);
        // If the timer detects that the server is shutdown, it will return an error, thus causing the try_join to end the future
        try_join!(timer_future, primary_port_future, tt_updater_future).map(|_| ()).map_err(|_| NetworkError::InternalError("Primary listener ended"))
    }

    async fn time_tracker_updater(tt: TimeTracker) -> Result<(), NetworkError> {
        let mut iter = tokio::time::interval_at(Instant::now() + NTP_RESYNC_FREQUENCY, NTP_RESYNC_FREQUENCY);
        while let Some(_) = iter.next().await {
            if !tt.resync().await {
                log::warn!("Unable to resynchronize NTP time (non-fatal; clock may diverge from synchronicity)");
            }
        }

        Ok(())
    }

    async fn primary_session_creator_loop(to_kernel: UnboundedSender<HdpServerResult>, session_manager: HdpSessionManager, mut socket: TcpListener) -> Result<(), NetworkError> {
        while let Some(stream) = socket.incoming().next().await {
            match stream {
                Ok(stream) => {
                    match stream.peer_addr() {
                        Ok(peer_addr) => {
                            stream.set_linger(Some(tokio::time::Duration::from_secs(0))).unwrap();
                            stream.set_keepalive(None).unwrap();
                            //stream.set_nodelay(true).unwrap();
                            // the below closure spawns a new future on the tokio thread pool
                            if let Err(err) = session_manager.process_new_inbound_connection(peer_addr, stream) {
                                to_kernel.unbounded_send(HdpServerResult::InternalServerError(None, format!("HDP Server dropping connection to {}. Reason: {}", peer_addr, err.to_string())))?;
                            }

                        }

                        Err(err) => {
                            log::error!("Error rendering peer addr: {}", err.to_string());
                        }
                    }
                }

                Err(err) => {
                    const WSACCEPT_ERROR: i32 = 10093;
                    if err.raw_os_error().unwrap_or(-1) != WSACCEPT_ERROR {
                        log::error!("Error accepting stream: {}", err.to_string());
                    }
                }
            }
        }

        Ok(())
    }

    async fn primary_timer(shutdown_receiver: tokio::sync::oneshot::Receiver<()>, _to_kernel: UnboundedSender<HdpServerResult>) -> Result<(), NetworkError> {
        shutdown_receiver.await.map_err(|_| NetworkError::InternalError("Shutdown receiver error"))
    }

    async fn outbound_kernel_request_handler(this: HdpServer, to_kernel_tx: UnboundedSender<HdpServerResult>, mut outbound_send_request_rx: UnboundedReceiver<(HdpServerRequest, Ticket)>) -> Result<(), NetworkError> {
        let read = inner!(this);
        let primary_port = read.local_bind_addr.port();
        //let port_start = read.multiport_range.start;
        let local_bind_addr = read.local_bind_addr.ip();
        let local_node_type = read.local_node_type;

        // We need only the underlying [HdpSessionManager]
        let mut session_manager = read.session_manager.clone();
        // Drop the read handle; we are done with it
        std::mem::drop(read);

        while let Some((outbound_request, ticket_id)) = outbound_send_request_rx.next().await {
            match outbound_request {
                HdpServerRequest::SendMessage(packet, implicated_cid, virtual_target, security_level) => {
                    if let Err(err) =  session_manager.process_outbound_packet(ticket_id, packet, implicated_cid, virtual_target, security_level) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::GroupBroadcastCommand(implicated_cid, cmd) => {
                    if let Err(err) =  session_manager.process_outbound_broadcast_command(ticket_id, implicated_cid, cmd) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::RegisterToHypernode(peer_addr, credentials, quantum_algorithm) => {
                    if let Err(err) = session_manager.initiate_connection(local_node_type, (local_bind_addr, primary_port), peer_addr, None, ticket_id, credentials, SecurityLevel::LOW, None, quantum_algorithm, None).await {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::ConnectToHypernode(peer_addr, implicated_cid, credentials, security_level, hdp_nodelay, quantum_algorithm, tcp_only) => {
                    if let Err(err) = session_manager.initiate_connection(local_node_type,(local_bind_addr, primary_port), peer_addr, Some(implicated_cid), ticket_id, credentials, security_level, hdp_nodelay, quantum_algorithm, tcp_only).await {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::DisconnectFromHypernode(implicated_cid, target) => {
                    if let Err(err) = session_manager.initiate_disconnect(implicated_cid, target, ticket_id) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::UpdateDrill(implicated_cid) => {
                    if let Err(err) = session_manager.initiate_update_drill_subroutine(implicated_cid, ticket_id) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::DeregisterFromHypernode(implicated_cid, virtual_connection_type) => {
                    if !session_manager.initiate_deregistration_subroutine(implicated_cid, virtual_connection_type, ticket_id) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), "CID not found".to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::PeerCommand(implicated_cid, peer_command) => {
                    if !session_manager.dispatch_peer_command(implicated_cid, ticket_id, peer_command) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), "CID not found".to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::SendFile(path, chunk_size, implicated_cid, virtual_target) => {
                    if let Err(err) = session_manager.process_outbound_file(ticket_id, chunk_size, path, implicated_cid, virtual_target, SecurityLevel::LOW) {
                        if let Err(_) = to_kernel_tx.unbounded_send(HdpServerResult::InternalServerError(Some(ticket_id), err.to_string())) {
                            return Err(NetworkError::InternalError("kernel disconnected from Hypernode instance"))
                        }
                    }
                }

                HdpServerRequest::Shutdown => {
                    let mut this = inner_mut!(this);
                    if let Some(tx) = this.shutdown_signaller.take() {
                        tx.send(()).unwrap_or(());
                    }
                    break;
                }
            }
        }

        Ok(())
    }
}

/// allows convenient communication with the server
#[derive(Clone)]
pub struct HdpServerRemote {
    outbound_send_request_tx: UnboundedSender<(HdpServerRequest, Ticket)>,
    ticket_counter: Arc<AtomicUsize>,
}


unsafe impl Send for HdpServerRemote {}
unsafe impl Sync for HdpServerRemote {}

impl Debug for HdpServerRemote {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "HdpServerRemote")
    }
}

impl HdpServerRemote {
    /// Creates a new [HdpServerRemote]
    pub fn new(outbound_send_request_tx: UnboundedSender<(HdpServerRequest, Ticket)>) -> Self {
        // starts at 1. Ticket 0 is for reserved
        Self { ticket_counter: Arc::new(AtomicUsize::new(1)), outbound_send_request_tx }
    }

    /// Sends a request to the HDP server. This should always be used to communicate with the server
    /// in order to obtain a ticket
    /// TODO: get rid of the unwrap
    pub fn unbounded_send(&self, request: HdpServerRequest) -> Result<Ticket, NetworkError> {
        let ticket = self.get_next_ticket();
        self.outbound_send_request_tx.unbounded_send((request, ticket))
            .map(|_| ticket)
            .map_err(|err| NetworkError::Generic(err.to_string()))
    }

    /// Especially used to keep track of a conversation (b/c a certain ticket number may be expected)
    pub fn send_with_custom_ticket(&self, ticket: Ticket, request: HdpServerRequest) -> Result<(), NetworkError> {
        self.outbound_send_request_tx.unbounded_send((request, ticket))
            .map_err(|err| NetworkError::Generic(err.to_string()))
    }

    /// Safely shutsdown the internal server
    pub fn shutdown(&self) -> Result<(), NetworkError> {
        let ticket = self.get_next_ticket();
        self.outbound_send_request_tx.unbounded_send((HdpServerRequest::Shutdown, ticket))
            .map_err(|err| NetworkError::Generic(err.to_string()))
    }


    fn get_next_ticket(&self) -> Ticket {
        Ticket(self.ticket_counter.fetch_add(1, SeqCst) as u64)
    }
}

impl Unpin for HdpServerRemote {}

impl Sink<(Ticket, HdpServerRequest)> for HdpServerRemote {
    type Error = NetworkError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: (Ticket, HdpServerRequest)) -> Result<(), Self::Error> {
        self.get_mut().send_with_custom_ticket(item.0, item.1)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// These are sent down the stack into the server. Most of the requests expect a ticket ID
/// in order for processes sitting above the [Kernel] to know how the request went
pub enum HdpServerRequest {
    /// Sends a request to the underlying [HdpSessionManager] to begin connecting to a new client
    RegisterToHypernode(SocketAddr, ProposedCredentials, Option<u8>),
    /// A high-level peer command. Can be used to facilitate communications between nodes in the HyperLAN
    PeerCommand(u64, PeerSignal),
    /// For submitting a de-register request
    DeregisterFromHypernode(u64, VirtualConnectionType),
    /// Send data to client. Peer addr, implicated cid, hdp_nodelay, quantum algorithm, tcp only
    ConnectToHypernode(SocketAddr, u64, ProposedCredentials, SecurityLevel, Option<bool>, Option<u8>, Option<bool>),
    /// Updates the drill for the given CID
    UpdateDrill(u64),
    /// Send data to an already existent connection
    SendMessage(SecBuffer, u64, VirtualTargetType, SecurityLevel),
    /// Send a file
    SendFile(PathBuf, Option<usize>, u64, VirtualTargetType),
    /// A group-message related command
    GroupBroadcastCommand(u64, GroupBroadcast),
    /// Tells the server to disconnect a session (implicated cid, target_cid)
    DisconnectFromHypernode(u64, VirtualConnectionType),
    /// shutdown signal
    Shutdown,
}

unsafe impl Send for HdpServerRequest {}
unsafe impl Sync for HdpServerRequest {}

/// This type is for relaying results between the lower-level server and the higher-level kernel
#[derive(Debug)]
pub enum HdpServerResult {
    /// Returns the CNAC which was created during the registration process
    RegisterOkay(Ticket, ClientNetworkAccount, Vec<u8>),
    /// The registration was a failure
    RegisterFailure(Ticket, String),
    /// When de-registration occurs. Third is_personal, Fourth is true if success, false otherwise
    DeRegistration(VirtualConnectionType, Option<Ticket>, bool, bool),
    /// Connection succeeded for the cid self.0. bool is "is personal"
    ConnectSuccess(Ticket, u64, SocketAddr, bool, VirtualConnectionType, String),
    /// The connection was a failure
    ConnectFail(Ticket, Option<u64>, String),
    /// The outbound request was rejected
    OutboundRequestRejected(Ticket, Option<Vec<u8>>),
    /// For file transfers. Implicated CID, Peer/Target CID, object ID
    FileTransferStatus(u64, FileKey, Ticket, FileTransferStatus),
    /// Data has been delivered for implicated cid self.0. The original outbound send request's ticket
    /// will be returned in the delivery, thus enabling higher-level abstractions to listen for data
    /// returns
    MessageDelivery(Ticket, u64, SecBuffer),
    /// Mailbox
    MailboxDelivery(u64, Option<Ticket>, MailboxTransfer),
    /// Peer result
    PeerEvent(PeerSignal, Ticket),
    /// for group-related events. Implicated cid, ticket, group info
    GroupEvent(u64, Ticket, GroupBroadcast),
    /// vt-cxn-type is optional, because it may have only been a provisional connection
    Disconnect(Ticket, u64, bool, Option<VirtualConnectionType>, String),
    /// An internal error occured
    InternalServerError(Option<Ticket>, String),
    /// A channel was created, with channel_id = ticket (same as post-connect ticket received)
    PeerChannelCreated(Ticket, PeerChannel)
}

/// A type sent through the server when a request is made
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, SerBin, DeBin)]
pub struct Ticket(pub u64);

impl Into<Ticket> for u64 {
    fn into(self) -> Ticket {
        Ticket(self)
    }
}

impl Into<Ticket> for usize {
    fn into(self) -> Ticket {
        Ticket(self as u64)
    }
}

impl Display for Ticket {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}