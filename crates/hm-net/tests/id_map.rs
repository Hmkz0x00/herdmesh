#![allow(clippy::unwrap_used, clippy::expect_used)]

use hm_net::{endpoint_id_from_node, node_id_from_endpoint};
use hm_proto::NodeId;
use iroh::SecretKey;

#[test]
fn iroh_endpoint_id_and_proto_node_id_are_the_same_ed25519_key() {
    for seed in [1u8, 42, 200] {
        let bytes = [seed; 32];
        let iroh_id = SecretKey::from_bytes(&bytes).public();
        let dalek = ed25519_dalek::SigningKey::from_bytes(&bytes);
        let proto_id = NodeId::from_verifying_key(&dalek.verifying_key());

        assert_eq!(iroh_id.as_bytes(), proto_id.as_bytes());
        assert_eq!(node_id_from_endpoint(iroh_id), proto_id);
        assert_eq!(endpoint_id_from_node(proto_id).unwrap(), iroh_id);
    }
}

#[test]
fn invalid_point_is_rejected() {
    // Not every 32-byte string is a valid ed25519 point.
    let bad = (0u8..=255)
        .map(|b| NodeId::from_bytes([b; 32]))
        .find(|n| endpoint_id_from_node(*n).is_err());
    assert!(bad.is_some(), "expected some invalid compressed point");
}
