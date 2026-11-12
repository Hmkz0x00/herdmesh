//! On-disk layout under the herdmesh data dir:
//!
//! ```text
//! <data>/
//!   herdmesh.toml            optional: relay_url = "https://..."
//!   node.key                 32-byte node secret (0600)
//!   rooms/<room_id hex>/
//!     room.toml              name, role, agent_name, secret_file
//!     secret.key             32-byte room secret (0600)
//!     peers.json             bootstrap EndpointAddrs (not secret)
//!     events.sqlite          hm-store event log
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};

use hm_net::{RelayChoice, RoomSecret};
use hm_proto::{Role, RoomId};
use iroh::{EndpointAddr, RelayUrl};
use serde::{Deserialize, Serialize};

/// Name of the per-room secret file, referenced from `room.toml`.
pub const SECRET_FILE: &str = "secret.key";

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {msg}")]
    Parse { path: PathBuf, msg: String },
    #[error("{path} is not a 32-byte secret")]
    SecretFormat { path: PathBuf },
    #[error("invalid relay_url {url:?}: {msg}")]
    RelayUrl { url: String, msg: String },
    #[error("no room matches {0:?}")]
    NoSuchRoom(String),
    #[error("{0:?} matches more than one room; use the room id")]
    AmbiguousRoom(String),
    #[error("a room named {0:?} already exists")]
    NameTaken(String),
    #[error("room {0} already exists locally")]
    RoomExists(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> DataError + '_ {
    move |source| DataError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Paths inside one data dir.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Same default as `herdmesh doctor`: `<platform data dir>/herdmesh`.
    pub fn default_root() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("herdmesh"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("herdmesh.toml")
    }
    pub fn node_key(&self) -> PathBuf {
        self.root.join("node.key")
    }
    pub fn rooms_dir(&self) -> PathBuf {
        self.root.join("rooms")
    }
    pub fn room_dir(&self, id: &RoomId) -> PathBuf {
        self.rooms_dir().join(id.to_string())
    }
    pub fn room_toml(&self, id: &RoomId) -> PathBuf {
        self.room_dir(id).join("room.toml")
    }
    pub fn room_secret(&self, id: &RoomId) -> PathBuf {
        self.room_dir(id).join(SECRET_FILE)
    }
    pub fn room_peers(&self, id: &RoomId) -> PathBuf {
        self.room_dir(id).join("peers.json")
    }
    pub fn events_db(&self, id: &RoomId) -> PathBuf {
        self.room_dir(id).join("events.sqlite")
    }

    pub fn load_config(&self) -> Result<Config, DataError> {
        let path = self.config_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => Config::parse(&s).map_err(|msg| DataError::Parse { path, msg }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// All locally known rooms, sorted by id.
    pub fn list_rooms(&self) -> Result<Vec<(RoomId, RoomConfig)>, DataError> {
        let dir = self.rooms_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(io_err(&dir)(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&dir))?;
            let Some(id) = entry.file_name().to_str().and_then(room_id_from_hex) else {
                continue;
            };
            if self.room_toml(&id).is_file() {
                out.push((id, self.load_room(&id)?));
            }
        }
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    /// Resolve a CLI `<room>` argument: exact name, full id, or unique id prefix.
    pub fn resolve_room(&self, arg: &str) -> Result<(RoomId, RoomConfig), DataError> {
        let rooms = self.list_rooms()?;
        let by_name: Vec<_> = rooms.iter().filter(|(_, c)| c.name == arg).collect();
        let hits: Vec<_> = if by_name.is_empty() {
            let a = arg.to_ascii_lowercase();
            rooms
                .iter()
                .filter(|(id, _)| !a.is_empty() && id.to_string().starts_with(&a))
                .collect()
        } else {
            by_name
        };
        match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(DataError::NoSuchRoom(arg.to_owned())),
            _ => Err(DataError::AmbiguousRoom(arg.to_owned())),
        }
    }

    pub fn load_room(&self, id: &RoomId) -> Result<RoomConfig, DataError> {
        let path = self.room_toml(id);
        let s = std::fs::read_to_string(&path).map_err(io_err(&path))?;
        toml::from_str(&s).map_err(|e| DataError::Parse {
            path,
            msg: e.to_string(),
        })
    }

    /// Create the room dir with config, secret and peers. Fails if the room
    /// exists or (for a non-empty name) the name is taken.
    pub fn create_room(
        &self,
        secret: &RoomSecret,
        cfg: &RoomConfig,
        peers: &[EndpointAddr],
    ) -> Result<RoomId, DataError> {
        let id = secret.room_id();
        if self.room_toml(&id).exists() {
            return Err(DataError::RoomExists(id.to_string()));
        }
        if self.list_rooms()?.iter().any(|(_, c)| c.name == cfg.name) {
            return Err(DataError::NameTaken(cfg.name.clone()));
        }
        let dir = self.room_dir(&id);
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        write_secret_file(&self.room_secret(&id), secret.as_bytes())?;
        self.save_peers(&id, peers)?;
        // room.toml last: its presence marks the room as complete.
        let path = self.room_toml(&id);
        let text = toml::to_string(cfg).map_err(|e| DataError::Parse {
            path: path.clone(),
            msg: e.to_string(),
        })?;
        std::fs::write(&path, text).map_err(io_err(&path))?;
        Ok(id)
    }

    pub fn load_secret(&self, id: &RoomId, cfg: &RoomConfig) -> Result<RoomSecret, DataError> {
        let path = self.room_dir(id).join(&cfg.secret_file);
        let bytes = std::fs::read(&path).map_err(io_err(&path))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| DataError::SecretFormat { path })?;
        Ok(RoomSecret::from_bytes(arr))
    }

    pub fn load_peers(&self, id: &RoomId) -> Result<Vec<EndpointAddr>, DataError> {
        let path = self.room_peers(id);
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| DataError::Parse {
                path,
                msg: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    pub fn save_peers(&self, id: &RoomId, peers: &[EndpointAddr]) -> Result<(), DataError> {
        let path = self.room_peers(id);
        let text = serde_json::to_vec_pretty(peers).map_err(|e| DataError::Parse {
            path: path.clone(),
            msg: e.to_string(),
        })?;
        std::fs::write(&path, text).map_err(io_err(&path))
    }

    pub fn remove_room(&self, id: &RoomId) -> Result<(), DataError> {
        let dir = self.room_dir(id);
        std::fs::remove_dir_all(&dir).map_err(io_err(&dir))
    }
}

/// Parse a 64-char lowercase hex room id (dir names). hm-proto has
/// `EventId::from_hex` but no `RoomId::from_hex` yet.
pub fn room_id_from_hex(s: &str) -> Option<RoomId> {
    if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(RoomId::from_bytes(out))
}

/// Write a secret file the way `node.key` is written: `create_new` (never
/// overwrite) and mode 0600 on Unix.
pub fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(io_err(path))?;
    f.write_all(bytes).map_err(io_err(path))?;
    f.sync_all().map_err(io_err(path))
}

/// `herdmesh.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Self-hosted relay; omitted = n0 public relays.
    pub relay_url: Option<String>,
}

impl Config {
    pub fn parse(s: &str) -> Result<Self, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }

    pub fn relay(&self) -> Result<RelayChoice, DataError> {
        match &self.relay_url {
            None => Ok(RelayChoice::N0),
            Some(url) => url
                .parse::<RelayUrl>()
                .map(RelayChoice::Custom)
                .map_err(|e| DataError::RelayUrl {
                    url: url.clone(),
                    msg: e.to_string(),
                }),
        }
    }
}

/// `rooms/<id>/room.toml`. Holds no secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomConfig {
    pub name: String,
    pub role: RoleName,
    /// Agent label used when this machine authors events in the room.
    pub agent_name: String,
    /// Secret file name, relative to the room dir.
    pub secret_file: String,
}

impl RoomConfig {
    pub fn new(name: impl Into<String>, role: RoleName, agent_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            role,
            agent_name: agent_name.into(),
            secret_file: SECRET_FILE.to_owned(),
        }
    }
}

/// Role as written in `room.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleName {
    Owner,
    Member,
    Observer,
}

impl From<RoleName> for Role {
    fn from(r: RoleName) -> Self {
        match r {
            RoleName::Owner => Role::Owner,
            RoleName::Member => Role::Member,
            RoleName::Observer => Role::Observer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dd() -> (tempfile::TempDir, DataDir) {
        let t = tempfile::tempdir().unwrap();
        let d = DataDir::new(t.path().join("hm"));
        (t, d)
    }

    #[test]
    fn layout_paths() {
        let d = DataDir::new("/x");
        let id = RoomSecret::from_bytes([1; 32]).room_id();
        let hex = id.to_string();
        assert_eq!(d.config_file(), Path::new("/x/herdmesh.toml"));
        assert_eq!(d.node_key(), Path::new("/x/node.key"));
        let room = Path::new("/x/rooms").join(&hex);
        assert_eq!(d.room_dir(&id), room);
        assert_eq!(d.room_toml(&id), room.join("room.toml"));
        assert_eq!(d.room_secret(&id), room.join("secret.key"));
        assert_eq!(d.room_peers(&id), room.join("peers.json"));
        assert_eq!(d.events_db(&id), room.join("events.sqlite"));
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn room_id_hex_roundtrip_and_rejects() {
        let id = RoomSecret::from_bytes([1; 32]).room_id();
        assert_eq!(room_id_from_hex(&id.to_string()), Some(id));
        assert_eq!(room_id_from_hex("abc"), None);
        assert_eq!(room_id_from_hex(&id.to_string().to_uppercase()), None);
        assert_eq!(room_id_from_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn config_missing_file_is_default_n0() {
        let (_t, d) = dd();
        let c = d.load_config().unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.relay().unwrap(), RelayChoice::N0);
    }

    #[test]
    fn config_relay_url_parses() {
        let c = Config::parse("relay_url = \"https://relay.example.com\"\n").unwrap();
        assert!(matches!(c.relay().unwrap(), RelayChoice::Custom(_)));
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn config_rejects_bad_input() {
        assert!(Config::parse("relay_url = 5").is_err());
        assert!(Config::parse("relay = \"x\"").is_err(), "unknown key");
        let c = Config::parse("relay_url = \"not a url\"").unwrap();
        assert!(matches!(c.relay(), Err(DataError::RelayUrl { .. })));
    }

    #[test]
    fn config_file_parse_error_names_path() {
        let (_t, d) = dd();
        std::fs::create_dir_all(d.root()).unwrap();
        std::fs::write(d.config_file(), "relay_url = [").unwrap();
        let e = d.load_config().unwrap_err().to_string();
        assert!(e.contains("herdmesh.toml"), "{e}");
    }

    #[test]
    fn room_toml_roundtrip_and_format() {
        let c = RoomConfig::new("alpha", RoleName::Owner, "lead");
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("role = \"owner\""), "{s}");
        assert!(s.contains("secret_file = \"secret.key\""), "{s}");
        assert_eq!(toml::from_str::<RoomConfig>(&s).unwrap(), c);
    }

    #[test]
    fn create_load_resolve_remove() {
        let (_t, d) = dd();
        let secret = RoomSecret::from_bytes([2; 32]);
        let cfg = RoomConfig::new("alpha", RoleName::Owner, "lead");
        let peer = EndpointAddr::new(iroh::SecretKey::from_bytes(&[3; 32]).public());
        let id = d
            .create_room(&secret, &cfg, std::slice::from_ref(&peer))
            .unwrap();

        assert_eq!(d.load_room(&id).unwrap(), cfg);
        assert_eq!(d.load_secret(&id, &cfg).unwrap(), secret);
        assert_eq!(d.load_peers(&id).unwrap(), vec![peer]);
        assert_eq!(std::fs::read(d.room_secret(&id)).unwrap(), [2; 32]);
        assert!(
            !std::fs::read_to_string(d.room_toml(&id))
                .unwrap()
                .contains("0202"),
            "room.toml must not hold the secret"
        );

        assert_eq!(d.resolve_room("alpha").unwrap().0, id);
        assert_eq!(d.resolve_room(&id.to_string()).unwrap().0, id);
        assert_eq!(d.resolve_room(&id.to_string()[..8]).unwrap().0, id);
        assert!(matches!(
            d.resolve_room("beta"),
            Err(DataError::NoSuchRoom(_))
        ));

        d.remove_room(&id).unwrap();
        assert!(d.list_rooms().unwrap().is_empty());
    }

    #[test]
    fn create_rejects_duplicates() {
        let (_t, d) = dd();
        let cfg = RoomConfig::new("alpha", RoleName::Owner, "lead");
        d.create_room(&RoomSecret::from_bytes([4; 32]), &cfg, &[])
            .unwrap();
        assert!(matches!(
            d.create_room(&RoomSecret::from_bytes([4; 32]), &cfg, &[]),
            Err(DataError::RoomExists(_))
        ));
        assert!(matches!(
            d.create_room(&RoomSecret::from_bytes([5; 32]), &cfg, &[]),
            Err(DataError::NameTaken(_))
        ));
    }

    #[test]
    fn ambiguous_prefix() {
        let (_t, d) = dd();
        // Find two seeds whose room ids share the first hex digit.
        let first = |b: u8| RoomSecret::from_bytes([b; 32]).room_id().to_string()[..1].to_owned();
        let (a, b) = (0u8..=255)
            .flat_map(|a| (a + 1..=255).map(move |b| (a, b)))
            .find(|&(a, b)| first(a) == first(b))
            .unwrap();
        for (i, s) in [a, b].into_iter().enumerate() {
            let cfg = RoomConfig::new(format!("r{i}"), RoleName::Member, "a");
            d.create_room(&RoomSecret::from_bytes([s; 32]), &cfg, &[])
                .unwrap();
        }
        assert!(matches!(
            d.resolve_room(&first(a)),
            Err(DataError::AmbiguousRoom(_))
        ));
        // An empty arg is not a wildcard.
        assert!(matches!(d.resolve_room(""), Err(DataError::NoSuchRoom(_))));
    }

    #[test]
    fn secret_file_never_overwritten() {
        let (_t, d) = dd();
        std::fs::create_dir_all(d.root()).unwrap();
        let p = d.root().join("s.key");
        write_secret_file(&p, &[1; 32]).unwrap();
        assert!(write_secret_file(&p, &[2; 32]).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), [1; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
