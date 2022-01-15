use hyxe_net::prelude::*;

/// Kernels for servers
pub mod server;
/// Kernels for clients
pub mod client;

/// A limited version of the ['HdpServerRemote'] designed to only allow shutdown calls to the protocol
#[derive(Clone)]
pub struct ShutdownRemote {
    pub(crate) inner: NodeRemote
}

impl ShutdownRemote {
    /// Gracefully closes the protocol and kernel executor
    pub async fn shutdown_kernel(mut self) -> Result<(), NetworkError> {
        self.inner.shutdown().await
    }
}