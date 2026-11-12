//! iroh `EndpointId` <-> hm-proto `NodeId`. Both are the same ed25519 public
//! key bytes (ADR-001 note 5); only the wrapper types differ.

use hm_proto::NodeId;
use iroh::{EndpointId, KeyParsingError};

/// Infallible: every iroh id is a valid ed25519 key.
pub fn node_id_from_endpoint(id: EndpointId) -> NodeId {
    NodeId::from_bytes(*id.as_bytes())
}

/// Fails if the bytes are not a valid compressed ed25519 point.
pub fn endpoint_id_from_node(id: NodeId) -> Result<EndpointId, KeyParsingError> {
    EndpointId::from_bytes(id.as_bytes())
}
