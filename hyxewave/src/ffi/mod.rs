use std::ops::Deref;
use std::sync::Arc;

use serde::Serialize;

use crate::command_handlers::connect::ConnectResponse;
use crate::command_handlers::disconnect::DisconnectResponse;
use crate::command_handlers::list_accounts::ActiveAccounts;
use crate::command_handlers::list_sessions::ActiveSessions;
use crate::command_handlers::peer::{PeerList, PostRegisterRequest, PostRegisterResponse, PeerMutuals, DeregisterResponse};
use crate::command_handlers::register::RegisterResponse;
use crate::console_error::ConsoleError;
use ser::string;
use hyxe_user::external_services::fcm::fcm_packet_processor::{FcmProcessorResult, FcmResult};
use hyxe_user::external_services::fcm::data_structures::FcmTicket;
use hyxe_user::external_services::fcm::data_structures::base64_string;
use hyxe_user::external_services::fcm::fcm_packet_processor::peer_post_register::FcmPostRegisterResponse;
use hyxe_user::misc::AccountError;

pub mod ffi_entry;

pub mod command_handler;
pub mod ser;

#[derive(Clone)]
pub struct FFIIO {
    // to send data from rust to native
    pub(crate) to_ffi_frontier: Arc<Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>>
}

impl Deref for FFIIO {
    type Target = Arc<Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>>;

    fn deref(&self) -> &Self::Target {
        &self.to_ffi_frontier
    }
}

impl From<Arc<Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>>> for FFIIO {
    fn from(to_ffi_frontier: Arc<Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>>) -> Self {
        Self { to_ffi_frontier }
    }
}

impl From<Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>> for FFIIO {
    fn from(to_ffi_frontier: Box<dyn Fn(Result<Option<KernelResponse>, ConsoleError>) + Send + Sync + 'static>) -> Self {
        Self { to_ffi_frontier: Arc::new(to_ffi_frontier) }
    }
}
// When this crate returns data to the FFI interface, the following combinations exist:
// We don't use tickets when passing between FFI Boundaries; we simply use the inner u64
// respresentation
#[derive(Debug, Serialize)]
#[serde(tag="type", content="info")]
#[allow(variant_size_differences)]
pub enum KernelResponse {
    Confirmation,
    Message(#[serde(with = "base64_string")] Vec<u8>),
    MessageReceived(#[serde(serialize_with = "string")] u64),
    KernelStatus(bool),
    // ticket, implicated_cid, icid (0 if HyperLAN server), peer_cid
    NodeMessage(#[serde(serialize_with = "string")] u64,#[serde(serialize_with = "string")] u64,#[serde(serialize_with = "string")] u64,#[serde(serialize_with = "string")] u64, #[serde(with = "base64_string")] Vec<u8>),
    ResponseTicket(#[serde(serialize_with = "string")] u64),
    ResponseFcmTicket(FcmTicket),
    ResponseHybrid(#[serde(serialize_with = "string")] u64, #[serde(with = "base64_string")] Vec<u8>),
    DomainSpecificResponse(DomainResponse),
    KernelShutdown(#[serde(with = "base64_string")] Vec<u8>),
    KernelInitiated,
    Error(#[serde(serialize_with = "string")] u64, #[serde(with = "base64_string")] Vec<u8>),
    FcmError(FcmTicket, #[serde(with = "base64_string")] Vec<u8>),
    Multiple(Vec<KernelResponse>)
}

impl KernelResponse {
    pub fn serialize_json(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&self).ok()
    }
}

// Some branches have a very specific return type. Handle these types with
#[derive(Debug, Serialize)]
#[serde(tag="dtype")]
pub enum DomainResponse {
    GetActiveSessions(ActiveSessions),
    GetAccounts(ActiveAccounts),
    Register(RegisterResponse),
    Connect(ConnectResponse),
    Disconnect(DisconnectResponse),
    PeerList(PeerList),
    PeerMutuals(PeerMutuals),
    PostRegisterRequest(PostRegisterRequest),
    PostRegisterResponse(PostRegisterResponse),
    DeregisterResponse(DeregisterResponse),
    FcmMessage(FcmMessage),
    FcmMessageSent(FcmMessageSent),
    FcmMessageReceived(FcmMessageReceived)
}

#[derive(Serialize, Debug)]
pub struct FcmMessage {
    pub fcm_ticket: FcmTicket,
    #[serde(with = "base64_string")]
    pub message: Vec<u8>
}

#[derive(Serialize, Debug)]
pub struct FcmMessageSent {
    pub fcm_ticket: FcmTicket
}

#[derive(Serialize, Debug)]
pub struct FcmMessageReceived {
    pub fcm_ticket: FcmTicket
}


impl From<Result<Option<KernelResponse>, ConsoleError>> for KernelResponse {
    fn from(res: Result<Option<KernelResponse>, ConsoleError>) -> Self {
        match res {
            Ok(resp_opt) => {
                match resp_opt {
                    Some(resp) => {
                        resp
                    }

                    None => {
                        KernelResponse::Confirmation
                    }
                }
            }

            Err(err) => {
                KernelResponse::Error(0, err.into_string().into_bytes())
            }
        }
    }
}

impl From<Result<FcmProcessorResult, AccountError>> for KernelResponse {
    fn from(res: Result<FcmProcessorResult, AccountError>) -> Self {
        match res {
            Err(err) => {
                KernelResponse::Error(0,err.into_string().into_bytes())
            }

            Ok(res) => {
                KernelResponse::from(res)
            }
        }
    }
}

impl From<FcmProcessorResult> for KernelResponse {
    fn from(res: FcmProcessorResult) -> Self {
        match res {
            FcmProcessorResult::Void | FcmProcessorResult::RequiresSave => {
                KernelResponse::Confirmation
            }

            FcmProcessorResult::Value(fcm_res) => {
                match fcm_res {
                    FcmResult::GroupHeader { ticket, message } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::FcmMessage(FcmMessage { fcm_ticket: ticket, message }))
                    }

                    FcmResult::GroupHeaderAck { ticket } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::FcmMessageReceived(FcmMessageReceived { fcm_ticket: ticket }))
                    }

                    FcmResult::MessageSent { ticket } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::FcmMessageSent(FcmMessageSent { fcm_ticket: ticket }))
                    }

                    FcmResult::Deregistered { peer_cid, requestor_cid, ticket } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::DeregisterResponse(DeregisterResponse {
                            implicated_cid: requestor_cid,
                            peer_cid,
                            ticket,
                            success: true
                        }))
                    }

                    FcmResult::PostRegisterInvitation { invite } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::PostRegisterRequest(PostRegisterRequest {
                            mail_id: 0,
                            username: invite.username,
                            peer_cid: invite.peer_cid,
                            implicated_cid: invite.local_cid,
                            ticket: invite.ticket,
                            fcm: true
                        }))
                    }

                    FcmResult::PostRegisterResponse { response: FcmPostRegisterResponse { local_cid, peer_cid, ticket, accept, username } } => {
                        KernelResponse::DomainSpecificResponse(DomainResponse::PostRegisterResponse(PostRegisterResponse {
                            implicated_cid: local_cid,
                            peer_cid,
                            ticket,
                            accept,
                            username: username.into_bytes(),
                            fcm: true
                        }))
                    }
                }
            }


            FcmProcessorResult::Values(vals) => {
                KernelResponse::Multiple(vals.into_iter().map(|r| KernelResponse::from(FcmProcessorResult::Value(r))).collect::<Vec<KernelResponse>>())
            }
        }
    }
}