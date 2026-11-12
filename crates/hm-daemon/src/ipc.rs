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
//! Same-user only, in four layers:
//! 1. OS access control on the endpoint: Windows pipe (first instance
//!    only) with an owner-only DACL that rejects remote clients; Unix
//!    socket (0600) inside a verified 0700 directory owned by us, never
//!    replacing a live socket, with the peer's uid checked on every
//!    connection where the OS reports it.
//! 2. The client checks who serves the name before writing anything
//!    ([`OwnerId`]): a listener run by another user is refused.
//! 3. Mutual challenge-response keyed by the secret [`Token`] (a private
//!    file, see [`load_or_create_token`]). The token never crosses the
//!    socket, and the client sends its request only after the server has
//!    proved it holds the token:
//!    `C→S ClientHello{nc}`, `S→C ServerHello{ns, mac(server, nc, ns)}`,
//!    `C→S ClientProof{mac(client, nc, ns)}`, `C→S Request`, `S→C Response`.
//! 4. Frame limits and a per-connection deadline.

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

/// IPC protocol version (first byte of every frame). 2: challenge-response.
pub const IPC_VERSION: u8 = 2;
/// Largest accepted frame payload.
pub const MAX_FRAME: usize = 1 << 20;
/// Whole-connection deadline (handshake, request, handler, response).
pub const CONN_DEADLINE: Duration = Duration::from_secs(10);
/// Secret length in bytes.
pub const TOKEN_LEN: usize = 32;
/// Handshake nonce length in bytes.
pub const NONCE_LEN: usize = 32;

/// Handshake nonce.
pub type Nonce = [u8; NONCE_LEN];
/// Handshake proof: `blake3::keyed_hash(token, role ‖ nc ‖ ns)`.
pub type Proof = [u8; 32];

const SERVER_ROLE: &[u8] = b"herdmesh-ipc-v2 server proof";
const CLIENT_ROLE: &[u8] = b"herdmesh-ipc-v2 client proof";

pub use crate::peer_owner::OwnerId;

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

    fn mac(&self, role: &[u8], client: &Nonce, server: &Nonce) -> Proof {
        let mut h = blake3::Hasher::new_keyed(&self.0);
        h.update(role);
        h.update(client);
        h.update(server);
        *h.finalize().as_bytes()
    }

    /// What the server sends to prove it holds the token.
    pub fn server_proof(&self, client: &Nonce, server: &Nonce) -> Proof {
        self.mac(SERVER_ROLE, client, server)
    }

    /// What the client sends to prove it holds the token.
    pub fn client_proof(&self, client: &Nonce, server: &Nonce) -> Proof {
        self.mac(CLIENT_ROLE, client, server)
    }
}

/// Constant-time proof comparison (`blake3::Hash` equality is).
fn proof_eq(a: &Proof, b: &Proof) -> bool {
    blake3::Hash::from(*a) == blake3::Hash::from(*b)
}

fn nonce() -> Result<Nonce, IpcError> {
    let mut n = [0u8; NONCE_LEN];
    getrandom::fill(&mut n).map_err(|_| IpcError::Random)?;
    Ok(n)
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
    #[error("socket is served by another user; refusing to talk to it")]
    NotOwner,
    #[error("random source unavailable")]
    Random,
    #[error("token file is not {TOKEN_LEN} bytes")]
    BadToken,
    #[error("token file is not owner-only: {0}")]
    InsecureToken(String),
}

/// First frame on every connection (client → server).
#[derive(Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub nonce: Nonce,
}

/// Server's reply: its nonce and proof it holds the token.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerHello {
    pub nonce: Nonce,
    pub proof: Proof,
}

/// Client's proof it holds the token; the request follows.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClientProof {
    pub proof: Proof,
}

/// Client half of the handshake. Returns only after the server proved it
/// holds `token`; the token itself is never written.
pub async fn client_handshake<S>(s: &mut S, token: &Token) -> Result<(), IpcError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let nc = nonce()?;
    write_frame(s, &ClientHello { nonce: nc }).await?;
    let sh: ServerHello = read_frame(s).await?;
    if !proof_eq(&sh.proof, &token.server_proof(&nc, &sh.nonce)) {
        return Err(IpcError::Refused);
    }
    write_frame(
        s,
        &ClientProof {
            proof: token.client_proof(&nc, &sh.nonce),
        },
    )
    .await
}

/// Server half of the handshake: `Ok` once the client proved it holds
/// `token`.
pub async fn server_handshake<S>(s: &mut S, token: &Token) -> Result<(), IpcError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let ch: ClientHello = read_frame(s).await?;
    let ns = nonce()?;
    write_frame(
        s,
        &ServerHello {
            nonce: ns,
            proof: token.server_proof(&ch.nonce, &ns),
        },
    )
    .await?;
    let cp: ClientProof = read_frame(s).await?;
    if !proof_eq(&cp.proof, &token.client_proof(&ch.nonce, &ns)) {
        return Err(IpcError::Refused);
    }
    Ok(())
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

/// Open a raw connection (no owner check, no handshake). For tests and
/// tooling that send hand-made frames.
#[doc(hidden)]
pub async fn connect_raw(name: &str) -> io::Result<Stream> {
    Stream::connect(to_name(name)?).await
}

/// Bind a bare listener on `name` (no DACL, no handshake), e.g. to play a
/// squatter in tests.
#[doc(hidden)]
pub fn bind_raw(name: &str) -> io::Result<Listener> {
    let opts = ListenerOptions::new().name(to_name(name)?);
    #[cfg(unix)]
    let opts = {
        ensure_private_dir(&socket_dir())?;
        opts.try_overwrite(true)
    };
    opts.create_tokio()
}

/// Who serves a socket name, judged without sending anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Nothing accepts connections there.
    Free,
    /// A process of the current user.
    Ours,
    /// Another user, or an owner we could not establish: do not talk to it.
    Foreign,
}

/// Connect to `name` and check who serves it; writes nothing.
pub async fn probe_owner(name: &str) -> Probe {
    let Ok(s) = connect_raw(name).await else {
        return Probe::Free;
    };
    match (OwnerId::current(), crate::peer_owner::server_owner(&s)) {
        (Ok(me), Ok(them)) if me == them => Probe::Ours,
        _ => Probe::Foreign,
    }
}

// ---- client ----

/// Connects per call (daemon restarts need no reconnect logic).
#[derive(Debug, Clone)]
pub struct Client {
    name: String,
    token: Token,
    /// Who must serve the name; `None` = the current user.
    owner: Option<OwnerId>,
}

impl Client {
    pub fn new(name: impl Into<String>, token: Token) -> Self {
        Self {
            name: name.into(),
            token,
            owner: None,
        }
    }

    /// Require a specific server identity instead of the current user's
    /// (tests simulate a foreign server this way).
    #[doc(hidden)]
    #[must_use]
    pub fn expect_owner(mut self, owner: OwnerId) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Check that our daemon (right user, same token) serves the name,
    /// without sending a request or our proof.
    pub async fn probe(&self) -> Result<(), IpcError> {
        timeout(CONN_DEADLINE, async {
            let mut s = self.connect_verified().await?;
            let nc = nonce()?;
            write_frame(&mut s, &ClientHello { nonce: nc }).await?;
            let sh: ServerHello = read_frame(&mut s).await?;
            if proof_eq(&sh.proof, &self.token.server_proof(&nc, &sh.nonce)) {
                Ok(())
            } else {
                Err(IpcError::Refused)
            }
        })
        .await
        .map_err(|_| IpcError::Deadline)?
    }

    /// Connect, and refuse (before writing anything) a server run by
    /// someone else.
    async fn connect_verified(&self) -> Result<Stream, IpcError> {
        let s = Stream::connect(to_name(&self.name)?).await?;
        let want = match &self.owner {
            Some(o) => o.clone(),
            None => OwnerId::current()?,
        };
        match crate::peer_owner::server_owner(&s) {
            Ok(got) if got == want => Ok(s),
            _ => Err(IpcError::NotOwner),
        }
    }

    /// Send `req`, return the daemon's response. Bounded by [`CONN_DEADLINE`].
    pub async fn request(&self, req: &Request) -> Result<Response, IpcError> {
        timeout(CONN_DEADLINE, self.request_inner(req))
            .await
            .map_err(|_| IpcError::Deadline)?
    }

    async fn request_inner(&self, req: &Request) -> Result<Response, IpcError> {
        let mut s = self.connect_verified().await?;
        client_handshake(&mut s, &self.token).await?;
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
            // First instance only: never replace a socket someone serves;
            // a stale file (nobody accepting) is replaced.
            let path = socket_dir().join(format!("{name}.sock"));
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(IpcError::Io(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "socket is already served",
                )));
            }
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
    server_handshake(&mut conn, token).await?;
    let resp = match read_frame::<_, Request>(&mut conn).await {
        Ok(req) => h.handle(req),
        Err(IpcError::Io(e)) => return Err(e.into()),
        // Wrong version, oversize, or garbage: refuse without parsing further.
        Err(_) => Response::Err(ErrCode::BadRequest),
    };
    write_frame(&mut conn, &resp).await
}
