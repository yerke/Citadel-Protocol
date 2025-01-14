use crate::prefabs::client::single_connection::SingleClientServerConnectionKernel;
use crate::prefabs::ClientServerRemote;
use crate::prelude::*;
use std::net::SocketAddr;
use uuid::Uuid;

/// A kernel that assists in creating and/or connecting to a group
pub mod broadcast;
/// A kernel that assists in allowing multiple possible peer-to-peer connections
pub mod peer_connection;
/// A kernel that only makes a single client-to-server connection
pub mod single_connection;

#[async_trait]
pub trait PrefabFunctions<'a, Arg: Send + 'a>: Sized {
    type UserLevelInputFunction: Send + 'a;
    /// Shared between the kernel and the on_c2s_channel_received function
    type SharedBundle: Send + 'a;

    fn get_shared_bundle(&self) -> Self::SharedBundle;

    async fn on_c2s_channel_received(
        connect_success: ConnectionSuccess,
        remote: ClientServerRemote,
        arg: Arg,
        fx: Self::UserLevelInputFunction,
        shared: Self::SharedBundle,
    ) -> Result<(), NetworkError>;

    fn construct(kernel: Box<dyn NetKernel + 'a>) -> Self;

    /// Creates a new connection with a central server entailed by the user information
    fn new_connect<T: Into<String>, P: Into<SecBuffer>>(
        username: T,
        password: P,
        arg: Arg,
        udp_mode: UdpMode,
        session_security_settings: SessionSecuritySettings,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server_conn_kernel = SingleClientServerConnectionKernel::new_connect(
            username,
            password,
            udp_mode,
            session_security_settings,
            |connect_success, remote| async move {
                let shared = rx
                    .await
                    .map_err(|err| NetworkError::Generic(err.to_string()))?;
                Self::on_c2s_channel_received(
                    connect_success,
                    remote,
                    arg,
                    on_channel_received,
                    shared,
                )
                .await
            },
        );

        let this = Self::construct(Box::new(server_conn_kernel));
        assert!(tx.send(this.get_shared_bundle()).is_ok());
        this
    }

    /// Crates a new connection with a central server entailed by the user information and default configuration
    fn new_connect_defaults<T: Into<String>, P: Into<SecBuffer>>(
        username: T,
        password: P,
        arg: Arg,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        Self::new_connect(
            username,
            password,
            arg,
            Default::default(),
            Default::default(),
            on_channel_received,
        )
    }

    /// First registers with a central server with the proposed credentials, and thereafter, establishes a connection with custom parameters
    #[allow(clippy::too_many_arguments)]
    fn new_register<T: Into<String>, R: Into<String>, P: Into<SecBuffer>>(
        full_name: T,
        username: R,
        password: P,
        arg: Arg,
        server_addr: SocketAddr,
        udp_mode: UdpMode,
        session_security_settings: SessionSecuritySettings,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server_conn_kernel = SingleClientServerConnectionKernel::new_register(
            full_name,
            username,
            password,
            server_addr,
            udp_mode,
            session_security_settings,
            |connect_success, remote| async move {
                let shared = rx
                    .await
                    .map_err(|err| NetworkError::Generic(err.to_string()))?;
                Self::on_c2s_channel_received(
                    connect_success,
                    remote,
                    arg,
                    on_channel_received,
                    shared,
                )
                .await
            },
        );

        let this = Self::construct(Box::new(server_conn_kernel));
        assert!(tx.send(this.get_shared_bundle()).is_ok());
        this
    }

    /// First registers with a central server with the proposed credentials, and thereafter, establishes a connection with default parameters
    fn new_register_defaults<T: Into<String>, R: Into<String>, P: Into<SecBuffer>>(
        full_name: T,
        username: R,
        password: P,
        arg: Arg,
        server_addr: SocketAddr,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        Self::new_register(
            full_name,
            username,
            password,
            arg,
            server_addr,
            Default::default(),
            Default::default(),
            on_channel_received,
        )
    }

    /// Creates a new authless connection with custom arguments
    fn new_passwordless(
        uuid: Uuid,
        server_addr: SocketAddr,
        arg: Arg,
        udp_mode: UdpMode,
        session_security_settings: SessionSecuritySettings,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server_conn_kernel = SingleClientServerConnectionKernel::new_passwordless(
            uuid,
            server_addr,
            udp_mode,
            session_security_settings,
            |connect_success, remote| async move {
                let shared = rx
                    .await
                    .map_err(|err| NetworkError::Generic(err.to_string()))?;
                Self::on_c2s_channel_received(
                    connect_success,
                    remote,
                    arg,
                    on_channel_received,
                    shared,
                )
                .await
            },
        );

        let this = Self::construct(Box::new(server_conn_kernel));
        assert!(tx.send(this.get_shared_bundle()).is_ok());
        this
    }

    /// Creates a new authless connection with default arguments
    fn new_passwordless_defaults(
        uuid: Uuid,
        server_addr: SocketAddr,
        arg: Arg,
        on_channel_received: Self::UserLevelInputFunction,
    ) -> Self {
        Self::new_passwordless(
            uuid,
            server_addr,
            arg,
            Default::default(),
            Default::default(),
            on_channel_received,
        )
    }
}
