//! SQLite-backed append-only event log for one room.
//!
//! Tables: `events` (keyed by EventId), `blobs` (bodies deduped by blake3),
//! `cursors` (local per-agent read cursors), `meta` (room, view snapshot).
//! Integers that take part in ordering are stored as big-endian BLOBs so
//! SQLite's memcmp order equals numeric order for the full u64 range.

use std::path::Path;

use hm_proto::{AgentRef, Event, EventBody, EventId, EventSig, Lamport, NodeId, RoomId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreError;
use crate::views::{ClaimPolicy, OrderKey, Views, order_key, order_only};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS blobs (
    hash BLOB PRIMARY KEY,
    body BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id          BLOB PRIMARY KEY,
    lamport     BLOB NOT NULL,
    author_node BLOB NOT NULL,
    agent_name  TEXT NOT NULL,
    ts          BLOB NOT NULL,
    body_hash   BLOB NOT NULL REFERENCES blobs(hash),
    sig         BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS events_order ON events (lamport, author_node, id);
CREATE TABLE IF NOT EXISTS cursors (
    node    BLOB NOT NULL,
    name    TEXT NOT NULL,
    lamport BLOB NOT NULL,
    PRIMARY KEY (node, name)
);
";

/// Bumped when the view snapshot layout changes; a mismatch triggers a rebuild.
const VIEW_SNAPSHOT_VERSION: u8 = 1;

/// Result of [`Store::append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Inserted,
    /// Same event id already stored; nothing changed.
    Duplicate,
}

/// Event log + materialized views for one room.
pub struct Store {
    conn: Connection,
    room: RoomId,
    views: Views,
    last_key: Option<OrderKey>,
    policy: ClaimPolicy,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("room", &self.room)
            .field("last_key", &self.last_key)
            .finish_non_exhaustive()
    }
}

fn be(n: u64) -> [u8; 8] {
    n.to_be_bytes()
}

fn arr<const N: usize>(v: Vec<u8>, what: &'static str) -> Result<[u8; N], StoreError> {
    v.try_into().map_err(|_| StoreError::Corrupt(what))
}

fn encode_key(k: &OrderKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(72);
    out.extend_from_slice(&be(k.0.0));
    out.extend_from_slice(k.1.as_bytes());
    out.extend_from_slice(k.2.as_bytes());
    out
}

fn decode_key(v: &[u8]) -> Option<OrderKey> {
    if v.len() != 72 {
        return None;
    }
    let lamport = u64::from_be_bytes(v[..8].try_into().ok()?);
    let node: [u8; 32] = v[8..40].try_into().ok()?;
    let id: [u8; 32] = v[40..72].try_into().ok()?;
    Some((
        Lamport(lamport),
        NodeId::from_bytes(node),
        EventId::from_bytes(id),
    ))
}

type RawRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, Vec<u8>, Vec<u8>, Vec<u8>);

/// All events with lamport > `since` (or all), in [`OrderKey`] order.
fn load_events(
    conn: &Connection,
    room: RoomId,
    since: Option<Lamport>,
) -> Result<Vec<Event>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.lamport, e.author_node, e.agent_name, e.ts, b.body, e.sig
         FROM events e JOIN blobs b ON b.hash = e.body_hash
         WHERE ?1 IS NULL OR e.lamport > ?1
         ORDER BY e.lamport, e.author_node, e.id",
    )?;
    let since = since.map(|l| be(l.0).to_vec());
    let rows = stmt.query_map(params![since], |r| {
        Ok::<RawRow, _>((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, lamport, node, name, ts, body, sig) = row?;
        let body: EventBody = postcard::from_bytes(&body)?;
        out.push(Event {
            id: EventId::from_bytes(arr(id, "event id")?),
            room,
            author: AgentRef::new(NodeId::from_bytes(arr(node, "author node")?), name),
            lamport: Lamport(u64::from_be_bytes(arr(lamport, "lamport")?)),
            ts: u64::from_be_bytes(arr(ts, "ts")?),
            body,
            sig: EventSig::from_bytes(arr(sig, "sig")?),
        });
    }
    Ok(out)
}

fn rebuild(
    conn: &Connection,
    room: RoomId,
    policy: ClaimPolicy,
) -> Result<(Views, Option<OrderKey>), StoreError> {
    let mut views = Views::default();
    let mut last = None;
    for e in load_events(conn, room, None)? {
        views.apply(&e, policy);
        last = Some(order_key(&e));
    }
    Ok((views, last))
}

fn save_views(conn: &Connection, views: &Views, last: Option<&OrderKey>) -> Result<(), StoreError> {
    let mut snap = vec![VIEW_SNAPSHOT_VERSION];
    snap.extend_from_slice(&postcard::to_allocvec(views)?);
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('views', ?1)",
        params![snap],
    )?;
    if let Some(k) = last {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_key', ?1)",
            params![encode_key(k)],
        )?;
    }
    Ok(())
}

fn load_snapshot(conn: &Connection) -> Result<Option<(Views, Option<OrderKey>)>, StoreError> {
    let get = |k: &str| -> Result<Option<Vec<u8>>, StoreError> {
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![k], |r| {
                r.get(0)
            })
            .optional()?)
    };
    let Some(snap) = get("views")? else {
        return Ok(None);
    };
    let views = match snap.split_first() {
        Some((&VIEW_SNAPSHOT_VERSION, rest)) => match postcard::from_bytes(rest) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };
    let last = match get("last_key")? {
        Some(k) => match decode_key(&k) {
            Some(k) => Some(k),
            None => return Ok(None),
        },
        None => None,
    };
    Ok(Some((views, last)))
}

impl Store {
    /// Open (or create) a file-backed store for `room`.
    pub fn open(path: impl AsRef<Path>, room: RoomId) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?, room)
    }

    /// Open a fresh in-memory store (tests, ephemeral peers).
    pub fn open_in_memory(room: RoomId) -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?, room)
    }

    fn init(conn: Connection, room: RoomId) -> Result<Self, StoreError> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        let stored: Option<Vec<u8>> = conn
            .query_row("SELECT value FROM meta WHERE key = 'room'", [], |r| {
                r.get(0)
            })
            .optional()?;
        match stored {
            Some(r) if r.as_slice() != room.as_bytes() => return Err(StoreError::WrongRoom),
            Some(_) => {}
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('room', ?1)",
                    params![room.as_bytes().to_vec()],
                )?;
            }
        }
        let policy: ClaimPolicy = order_only;
        let (views, last_key) = match load_snapshot(&conn)? {
            Some(s) => s,
            None => rebuild(&conn, room, policy)?,
        };
        Ok(Self {
            conn,
            room,
            views,
            last_key,
            policy,
        })
    }

    pub fn room(&self) -> RoomId {
        self.room
    }

    /// Current materialized views.
    pub fn views(&self) -> &Views {
        &self.views
    }

    /// Verify and append. Idempotent on duplicate id. Views update incrementally
    /// when the event sorts after everything seen, otherwise by full rebuild.
    pub fn append(&mut self, e: &Event) -> Result<AppendOutcome, StoreError> {
        if e.room != self.room {
            return Err(StoreError::WrongRoom);
        }
        e.verify()?;
        let body = postcard::to_allocvec(&e.body)?;
        let hash = blake3::hash(&body);

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO blobs (hash, body) VALUES (?1, ?2)",
            params![hash.as_bytes().to_vec(), body],
        )?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO events (id, lamport, author_node, agent_name, ts, body_hash, sig)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id.as_bytes().to_vec(),
                be(e.lamport.0).to_vec(),
                e.author.node.as_bytes().to_vec(),
                e.author.agent_name,
                be(e.ts).to_vec(),
                hash.as_bytes().to_vec(),
                e.sig.as_bytes().to_vec(),
            ],
        )?;
        if inserted == 0 {
            return Ok(AppendOutcome::Duplicate); // tx drops → rollback (no-op)
        }

        let key = order_key(e);
        let (views, last_key) = if self.last_key.is_none_or(|last| key > last) {
            let mut v = self.views.clone();
            v.apply(e, self.policy);
            (v, Some(key))
        } else {
            rebuild(&tx, self.room, self.policy)?
        };
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(AppendOutcome::Inserted)
    }

    /// Recompute all views from the log.
    pub fn rebuild_views(&mut self) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let (views, last_key) = rebuild(&tx, self.room, self.policy)?;
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(())
    }

    /// Lamport for the next locally authored event: `max(seen) + 1`.
    pub fn next_lamport(&self) -> Result<Lamport, StoreError> {
        let max: Option<Vec<u8>> =
            self.conn
                .query_row("SELECT MAX(lamport) FROM events", [], |r| r.get(0))?;
        let seen = max
            .map(|m| arr::<8>(m, "lamport").map(|b| Lamport(u64::from_be_bytes(b))))
            .transpose()?;
        Ok(crate::views::next_lamport(seen))
    }

    /// Events with lamport strictly greater than `since`, in order.
    pub fn events_since(&self, since: Lamport) -> Result<Vec<Event>, StoreError> {
        load_events(&self.conn, self.room, Some(since))
    }

    /// Local read cursor for `agent`, if any.
    pub fn cursor(&self, agent: &AgentRef) -> Result<Option<Lamport>, StoreError> {
        let v: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT lamport FROM cursors WHERE node = ?1 AND name = ?2",
                params![agent.node.as_bytes().to_vec(), agent.agent_name],
                |r| r.get(0),
            )
            .optional()?;
        v.map(|b| arr::<8>(b, "cursor").map(|b| Lamport(u64::from_be_bytes(b))))
            .transpose()
    }

    /// Move `agent`'s cursor forward to `to`. Never moves it back.
    pub fn advance_cursor(&mut self, agent: &AgentRef, to: Lamport) -> Result<(), StoreError> {
        if self.cursor(agent)?.is_some_and(|cur| cur >= to) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO cursors (node, name, lamport) VALUES (?1, ?2, ?3)",
            params![
                agent.node.as_bytes().to_vec(),
                agent.agent_name,
                be(to.0).to_vec()
            ],
        )?;
        Ok(())
    }

    pub fn event_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(n.unsigned_abs())
    }

    pub fn blob_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        Ok(n.unsigned_abs())
    }
}
