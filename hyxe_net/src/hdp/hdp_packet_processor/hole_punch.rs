use super::includes::*;
use crate::hdp::hdp_packet_processor::primary_group_packet::{get_resp_target_cid_from_header, get_proper_hyper_ratchet};
use crate::error::NetworkError;

/// This will handle an inbound group packet
pub fn process(session: &HdpSession, packet: HdpPacket, hr_version: u32, proxy_cid_info: Option<(u64, u64)>) -> Result<PrimaryProcessorResult, NetworkError> {
    let (header, payload, _, _) = packet.decompose();
    let state_container = inner_state!(session.state_container);
    let ref cnac = return_if_none!(state_container.cnac.clone(), "CNAC not loaded");
    let ref hr = return_if_none!(get_proper_hyper_ratchet(hr_version, cnac, &state_container, proxy_cid_info), "Unable to get proper HR");

    let header = header.as_ref();
    let (header, payload) = return_if_none!(super::super::validation::aead::validate_custom(hr, &header, payload), "Unable to validate packet");
    log::info!("Success validating hole-punch packet");
    let peer_cid = get_resp_target_cid_from_header(&header);
    return_if_none!(return_if_none!(state_container.hole_puncher_pipes.get(&peer_cid), "Unable to get hole puncher pipe").send(payload.freeze()).ok(), "Unable to forward hole-punch packet through pipe");
    log::info!("Success forwarding hole-punch packet to hole-puncher");

    Ok(PrimaryProcessorResult::Void)
}
