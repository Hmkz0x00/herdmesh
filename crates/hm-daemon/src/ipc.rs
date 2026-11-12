//! Local IPC between agent adapters (`hm-mcp`) and the daemon.
//!
//! Transport: an `interprocess` local socket (named pipe on Windows, Unix
//! domain socket elsewhere) named per user. Every connection serves exactly
//! one request/response pair inside [`CONN_DEADLINE`].
//!
//! Frame: `version u8 | len u32 big-endian | postcard payload`, `len` ≤
//! [`MAX_FRAME`]. A frame with a wrong version or an oversize length is
//! refused before its body is read.
//!
//! Same-user only, in three layers:
//! 1. OS access control on the endpoint: Windows pipe with an owner-only
//!    DACL that rejects remote clients; Unix socket (0600) inside a
//!    verified 0700 directory owned by us, with the peer's uid checked on
//!    every connection where the OS reports it.
//! 2. The first frame must be a [`Hello`] carrying the secret [`Token`],
//!    which lives in a private file (see [`load_or_create_token`]).
//! 3. Frame limits and a per-connection deadline.

use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hm_proto::RoomId;
use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(not(unix))]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name};
#[cfg(unix)]
use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
#[cfg(windows)]
use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// IPC protocol version (first byte of every frame).
pub const IPC_VERSION: u8 = 1;
/// Largest accepted frame payload.
pub const MAX_FRAME: usize = 1 << 20;
/// Whole-connection deadline (handshake, request, handler, response).
pub const CONN_DEADLINE: Duration = Duration::from_secs(10);
/// Secret length in bytes.
pub const TOKEN_LEN: usize = 32;

/// Shared secret proving the client can read the user's private token file.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; TOKEN_LEN]);

impl Token {
    pub fn from_bytes(b: [u8; TOKEN_LEN]) -> Self {
        Self(b)
    }
    pub fn random() -> Result<Self, IpcError> {
        let mut b = [0u8; TOKEN_LEN];
        getrandom::fill(&mut b).map_err(|_| IpcError::Random)?;
        Ok(Self(b))
    }
    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self.0
    }
    /// Constant-time comparison (blake3 hash equality).
    fn matches(&self, other: &[u8; TOKEN_LEN]) -> bool {
        blake3::hash(&self.0) == blake3::hash(other)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Read the token at `path`, creating it (random, owner-only) if absent.
/// An existing file that others can access is refused.
pub fn load_or_create_token(path: &Path) -> Result<Token, IpcError> {
    match std::fs::read(path) {
        Ok(b) => {
            use hm_net::secure_file::check_private;
            match check_private(path) {
                Ok(None) => {}
                Ok(Some(why)) => return Err(IpcError::InsecureToken(why)),
                Err(e) => return Err(IpcError::InsecureToken(e.to_string())),
            }
            let a: [u8; TOKEN_LEN] = b.try_into().map_err(|_| IpcError::BadToken)?;
            Ok(Token(a))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let t = Token::random()?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = hm_net::secure_file::create_private(path)
                .map_err(|e| IpcError::Io(io::Error::other(e.to_string())))?;
            io::Write::write_all(&mut f, &t.0)?;
            Ok(t)
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("codec: {0}")]
    Codec(#[from] postcard::Error),
    #[error("bad frame: {0}")]
    BadFrame(&'static str),
    #[error("deadline exceeded")]
    Deadline,
    #[error("refused")]
    Refused,
    #[error("random source unavailable")]
    Random,
    #[error("token file is not {TOKEN_LEN} bytes")]
    BadToken,
    #[error("token file is not owner-only: {0}")]
    InsecureToken(String),
}

/// First frame on every connection.
#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub token: [u8; TOKEN_LEN],
}

/// Why a request failed. Never carries peer text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrCode {
    BadRequest,
    Forbidden,
    NotFound,
    NoRoom,
    Internal,
}

/// `(author label, text)` pairs; the MCP layer wraps them as untrusted.
pub type Items = Vec<(String, String)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Ok(Items),
    Err(ErrCode),
}

/// One daemon call; mirrors `hm_mcp::Backend`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Call {
    RoomCard,
    Brief {
        max_tokens: u32,
    },
    Inbox {
        ack: bool,
    },
    Ask {
        to: String,
        question: String,
    },
    Answer {
        ask_id: String,
        text: String,
    },
    Handoff {
        to: Option<String>,
        summary: String,
        refs: Vec<String>,
        next_steps: Vec<String>,
    },
    Claim {
        paths: Vec<String>,
        ttl_min: u32,
    },
    Release {
        paths: Vec<String>,
    },
    TaskAdd {
        task: String,
        title: String,
        paths: Vec<String>,
    },
    TaskUpdate {
        task: String,
        status: String,
        note: Option<String>,
    },
    TaskList {
        filter: Option<String>,
    },
    Decide {
        title: String,
        rationale: String,
        refs: Vec<String>,
    },
    GetEvent {
        id: String,
    },
    Search {
        query: String,
        limit: u32,
    },
}

/// A call tagged with the calling agent's identity (its name; the daemon
/// supplies the node key) and, optionally, a room (default room if `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub agent: String,
    pub room: Option<RoomId>,
    pub call: Call,
}

// ---- framing ----

/// Write one frame.
pub async fn write_frame<W: AsyncWriteExt + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), IpcError> {
    let body = postcard::to_allocvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(IpcError::BadFrame("oversize"));
    }
    let mut buf = Vec::with_capacity(5 + body.len());
    buf.push(IPC_VERSION);
    buf.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame's payload, checking version and length before the body.
pub async fn read_payload<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Vec<u8>, IpcError> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).await?;
    if head[0] != IPC_VERSION {
        return Err(IpcError::BadFrame("version"));
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(IpcError::BadFrame("length"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

/// Read and decode one frame.
pub async fn read_frame<R: AsyncReadExt + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<T, IpcError> {
    Ok(postcard::from_bytes(&read_payload(r).await?)?)
}

// ---- names ----

/// Per-user socket name, e.g. `herdmesh-alice`.
pub fn socket_name(user: &str) -> String {
    let clean: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("herdmesh-{clean}")
}

/// Current OS user, best effort.
pub fn current_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_owned())
}

/// Directory holding the Unix socket: `$XDG_RUNTIME_DIR/herdmesh` (already
/// per-user and 0700 on systemd hosts), else a per-user dir under the temp
/// dir. [`ensure_private_dir`] verifies it before the daemon binds.
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("herdmesh"),
        _ => std::env::temp_dir().join(format!("herdmesh-{}", socket_name(&current_user()))),
    }
}

/// Create `dir` (0700) if missing, then insist it is a real directory, not
/// accessible by group/other, and owned by us. Returns our uid.
///
/// Ownership is proved without libc: a file we create in the directory gets
/// our uid, and must match the directory's owner (we could not create it in
/// someone else's 0700 directory at all).
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> io::Result<u32> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
    let denied = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: {why}", dir.display()),
        )
    };
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Err(e) => return Err(e),
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() {
        return Err(denied("not a real directory"));
    }
    if md.mode() & 0o077 != 0 {
        return Err(denied("accessible by group/other (want 0700)"));
    }
    let probe = dir.join(format!(".probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .map_err(|_| denied("cannot write here: not our directory"))?;
    let uid = f.metadata()?.uid();
    drop(f);
    let _ = std::fs::remove_file(&probe);
    if uid != md.uid() {
        return Err(denied("owned by another user"));
    }
    Ok(uid)
}

fn to_name(name: &str) -> io::Result<Name<'static>> {
    if name.is_empty() || name.contains(['/', '\\', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad socket name",
        ));
    }
    #[cfg(unix)]
    {
        socket_dir()
            .join(format!("{name}.sock"))
            .to_fs_name::<GenericFilePath>()
    }
    #[cfg(not(unix))]
    {
        name.to_owned().to_ns_name::<GenericNamespaced>()
    }
}

/// Windows: a DACL with a single full-access ACE for the current user, so
/// no other local user (or service) can open the pipe.
#[cfg(windows)]
fn owner_only_descriptor()
-> io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    let sid = hm_net::secure_file::current_user_sid()?;
    let sddl = widestring::U16CString::from_str(format!("D:P(A;;GA;;;{sid})"))
        .map_err(io::Error::other)?;
    interprocess::os::windows::security_descriptor::SecurityDescriptor::deserialize(&sddl)
}

/// Open a raw connection (no handshake). For tests and tooling that send
/// hand-made frames.
#[doc(hidden)]
pub async fn connect_raw(name: &str) -> io::Result<Stream> {
    Stream::connect(to_name(name)?).await
}

// ---- client ----

/// Connects per call (daemon restarts need no reconnect logic).
#[derive(Debug, Clone)]
pub struct Client {
    name: String,
    token: Token,
}

impl Client {
    pub fn new(name: impl Into<String>, token: Token) -> Self {
        Self {
            name: name.into(),
            token,
        }
    }

    /// Send `req`, return the daemon's response. Bounded by [`CONN_DEADLINE`].
    pub async fn request(&self, req: &Request) -> Result<Response, IpcError> {
        timeout(CONN_DEADLINE, self.request_inner(req))
            .await
            .map_err(|_| IpcError::Deadline)?
    }

    async fn request_inner(&self, req: &Request) -> Result<Response, IpcError> {
        let mut s = Stream::connect(to_name(&self.name)?).await?;
        write_frame(
            &mut s,
            &Hello {
                token: self.token.0,
            },
        )
        .await?;
        write_frame(&mut s, req).await?;
        read_frame(&mut s).await
    }
}

// ---- server ----

/// Answers decoded requests. Called from async tasks; keep it quick.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> Response;
}

/// A bound local socket.
pub struct Server {
    listener: Listener,
    token: Token,
    /// Unix: uid every peer must have (our own). `None` elsewhere.
    owner_uid: Option<u32>,
}

/// Aborts the accept loop on drop.
#[derive(Debug)]
pub struct ServerHandle(JoinHandle<()>);

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Server {
    pub fn bind(name: &str, token: Token) -> Result<Self, IpcError> {
        let opts = ListenerOptions::new().name(to_name(name)?);
        // Remote pipe clients are rejected by interprocess's default
        // (`accept_remote == false` => PIPE_REJECT_REMOTE_CLIENTS).
        #[cfg(windows)]
        let opts = opts.security_descriptor(owner_only_descriptor()?);
        #[cfg(unix)]
        let (opts, owner_uid) = {
            let uid = ensure_private_dir(&socket_dir())?;
            // Callers probe that nothing serves the name: replace a stale file.
            (opts.try_overwrite(true).mode(0o600), Some(uid))
        };
        #[cfg(not(unix))]
        let owner_uid = None;
        let listener = opts.create_tokio()?;
        Ok(Self {
            listener,
            token,
            owner_uid,
        })
    }

    /// Serve until the returned handle is dropped.
    pub fn spawn(self, handler: Arc<dyn Handler>) -> ServerHandle {
        let Self {
            listener,
            token,
            owner_uid,
        } = self;
        let token = Arc::new(token);
        ServerHandle(tokio::spawn(async move {
            loop {
                let Ok(conn) = listener.accept().await else {
                    continue;
                };
                if let Some(uid) = owner_uid
                    && !peer_is_owner(&conn, uid)
                {
                    continue; // dropped: wrong user, or credentials unreadable
                }
                let (h, t) = (handler.clone(), token.clone());
                tokio::spawn(async move {
                    // Errors end the connection; there is nobody to tell.
                    let _ = timeout(CONN_DEADLINE, serve_conn(conn, &t, h.as_ref())).await;
                });
            }
        }))
    }
}

/// Unix: the peer's effective uid must be ours. Platforms that cannot report
/// it (`euid() == None`) rely on the directory mode and the token.
#[cfg(unix)]
fn peer_is_owner(conn: &Stream, uid: u32) -> bool {
    match conn.peer_creds() {
        Ok(c) => c.euid().is_none_or(|peer| peer == uid),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn peer_is_owner(_: &Stream, _: u32) -> bool {
    true
}

async fn serve_conn(mut conn: Stream, token: &Token, h: &dyn Handler) -> Result<(), IpcError> {
    let hello: Hello = read_frame(&mut conn).await?;
    if !token.matches(&hello.token) {
        return Err(IpcError::Refused);
    }
    let resp = match read_frame::<_, Request>(&mut conn).await {
        Ok(req) => h.handle(req),
        Err(IpcError::Io(e)) => return Err(e.into()),
        // Wrong version, oversize, or garbage: refuse without parsing further.
        Err(_) => Response::Err(ErrCode::BadRequest),
    };
    write_frame(&mut conn, &resp).await
}
