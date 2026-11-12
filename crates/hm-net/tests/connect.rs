//! Two endpoints, relay disabled, connect on localhost over our ALPN.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hm_net::{Net, NetConfig, RelayChoice, SYNC_ALPN, node_id_from_endpoint};

async fn bind(dir: &tempfile::TempDir, name: &str) -> Net {
    Net::bind(NetConfig {
        key_path: dir.path().join(name).join("node.key"),
        relay: RelayChoice::Disabled,
        allow_insecure_key: false,
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_connect_over_sync_alpn() {
    let dir = tempfile::tempdir().unwrap();
    let a = bind(&dir, "a").await;
    let b = bind(&dir, "b").await;

    let conn = tokio::time::timeout(
        Duration::from_secs(20),
        a.endpoint().connect(b.local_addr(), SYNC_ALPN),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    assert_eq!(conn.alpn(), SYNC_ALPN);
    assert_eq!(node_id_from_endpoint(conn.remote_id()), b.node_id());

    // The stub handler refuses the protocol by closing the connection.
    tokio::time::timeout(Duration::from_secs(20), conn.closed())
        .await
        .expect("stub did not close");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn key_is_persisted_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let first = bind(&dir, "a").await;
    let id = first.node_id();
    first.shutdown().await;
    let again = bind(&dir, "a").await;
    assert_eq!(again.node_id(), id);
    again.shutdown().await;
}

#[test]
fn sync_alpn_is_reserved_name() {
    assert_eq!(SYNC_ALPN, b"herdmesh/sync/1");
}
