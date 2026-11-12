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
//! Same-user only: the first frame must be a [`Hello`] carrying the secret
//! [`Token`], which lives in a private file (see [`load_or_create_token`]).
//! Other users cannot read that file, so they cannot talk to the daemon.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hm_proto::RoomId;
use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, ToFsName, ToNsName,
};
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
pub fn load_or_create_token(path: &Path) -> Result<Token, IpcError> {
    match std::fs::read(path) {
        Ok(b) => {
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

fn to_name(name: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        name.to_owned().to_ns_name::<GenericNamespaced>()
    } else {
        let p = std::env::temp_dir().join(format!("{name}.sock"));
        p.to_fs_name::<GenericFilePath>()
    }
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
        let listener = ListenerOptions::new().name(to_name(name)?).create_tokio()?;
        Ok(Self { listener, token })
    }

    /// Serve until the returned handle is dropped.
    pub fn spawn(self, handler: Arc<dyn Handler>) -> ServerHandle {
        let Self { listener, token } = self;
        let token = Arc::new(token);
        ServerHandle(tokio::spawn(async move {
            loop {
                let Ok(conn) = listener.accept().await else {
                    continue;
                };
                let (h, t) = (handler.clone(), token.clone());
                tokio::spawn(async move {
                    // Errors end the connection; there is nobody to tell.
                    let _ = timeout(CONN_DEADLINE, serve_conn(conn, &t, h.as_ref())).await;
                });
            }
        }))
    }
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
