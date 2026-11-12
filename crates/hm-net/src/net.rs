//! iroh endpoint + router: gossip and the reserved `herdmesh/sync/1` ALPN.

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use hm_proto::NodeId;
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};
use iroh_gossip::Gossip;

use crate::id::node_id_from_endpoint;

/// ALPN for request/response sync between members (handler lands in M2-x).
pub const SYNC_ALPN: &[u8] = b"herdmesh/sync/1";

/// Which relay servers to use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RelayChoice {
    /// n0's public relays and DNS discovery.
    #[default]
    N0,
    /// A self-hosted relay; n0 DNS discovery is still used.
    Custom(RelayUrl),
    /// Direct connections only, no relay, no discovery. For tests / LAN.
    Disabled,
}

/// How to build a [`Net`].
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// File holding the 32-byte node secret key; created if missing.
    pub key_path: PathBuf,
    pub relay: RelayChoice,
}

/// Errors from [`Net`].
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("key file {path}: {source}")]
    KeyIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("key file {path} is not a 32-byte key")]
    KeyFormat { path: PathBuf },
    #[error("binding endpoint: {0}")]
    Bind(#[from] iroh::endpoint::BindError),
}

/// Load the node secret key from `path`, generating and saving one if absent.
///
/// The file is created with `create_new`, so an existing key is never
/// overwritten. On Unix it is mode 0600.
pub fn load_or_create_key(path: &Path) -> Result<SecretKey, NetError> {
    let io = |source| NetError::KeyIo {
        path: path.to_owned(),
        source,
    };
    match std::fs::read(path) {
        Ok(bytes) => {
            let arr: [u8; 32] = bytes.try_into().map_err(|_| NetError::KeyFormat {
                path: path.to_owned(),
            })?;
            Ok(SecretKey::from_bytes(&arr))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            let key = SecretKey::generate();
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                opts.mode(0o600);
            }
            let mut f = opts.open(path).map_err(io)?;
            f.write_all(&key.to_bytes()).map_err(io)?;
            f.sync_all().map_err(io)?;
            tracing::info!(path = %path.display(), "generated node key");
            Ok(key)
        }
        Err(e) => Err(io(e)),
    }
}

/// Stub for the reserved sync ALPN: refuses every connection for now.
#[derive(Debug, Clone)]
struct SyncStub;

impl ProtocolHandler for SyncStub {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        conn.close(1u32.into(), b"herdmesh/sync/1 unimplemented");
        Err(AcceptError::from_err(std::io::Error::other(
            "herdmesh/sync/1 unimplemented",
        )))
    }
}

/// A bound iroh endpoint with gossip and the sync ALPN mounted.
#[derive(Debug, Clone)]
pub struct Net {
    router: Router,
    gossip: Gossip,
}

impl Net {
    pub async fn bind(cfg: NetConfig) -> Result<Self, NetError> {
        let key = load_or_create_key(&cfg.key_path)?;
        let builder = match cfg.relay {
            RelayChoice::N0 => Endpoint::builder(presets::N0),
            RelayChoice::Custom(url) => {
                Endpoint::builder(presets::N0).relay_mode(RelayMode::custom([url]))
            }
            // Minimal: crypto provider only, so no DNS/pkarr traffic either.
            RelayChoice::Disabled => {
                Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
            }
        };
        let endpoint = builder.secret_key(key).bind().await?;
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(SYNC_ALPN, SyncStub)
            .spawn();
        tracing::info!(node = %router.endpoint().id(), "endpoint bound");
        Ok(Self { router, gossip })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn gossip(&self) -> &Gossip {
        &self.gossip
    }

    pub fn node_id(&self) -> NodeId {
        node_id_from_endpoint(self.endpoint().id())
    }

    /// Our address as currently known (relay URL and/or direct addresses).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint().addr()
    }

    /// Our bound sockets rewritten to loopback; for same-host peers and tests.
    pub fn local_addr(&self) -> EndpointAddr {
        let ips = self.endpoint().bound_sockets().into_iter().map(|s| {
            let ip = match s.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            };
            SocketAddr::new(ip, s.port())
        });
        ips.fold(EndpointAddr::new(self.endpoint().id()), |a, s| {
            a.with_ip_addr(s)
        })
    }

    /// Stop accepting and close the endpoint.
    pub async fn shutdown(&self) {
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!(error = %e, "router shutdown");
        }
    }
}
