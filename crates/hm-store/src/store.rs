//! SQLite-backed append-only event log for one room.
//!
//! Tables: `events` (keyed by EventId), `blobs` (bodies deduped by blake3),
//! `cursors` (local per-agent read cursors), `meta` (room, view snapshot).
//! `events.received_at` is local wall-clock arrival time (NULL for rows from
//! before it existed); lease times use `min(ts, received_at)`.
//! Integers that take part in ordering are stored as big-endian BLOBs so
//! SQLite's memcmp order equals numeric order for the full u64 range.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hm_proto::{AgentRef, Event, EventBody, EventId, EventSig, Lamport, NodeId, RoomId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreError;
use crate::settle::{self, ClaimState};
use crate::views::{ClaimPolicy, OrderKey, Views, effective_ts, order_key, order_only};

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
    sig         BLOB NOT NULL,
    received_at BLOB
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
const VIEW_SNAPSHOT_VERSION: u8 = 2;

/// Result of [`Store::append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Inserted,
    /// Same event id already stored; nothing changed.
    Duplicate,
}

/// Result of [`Store::append_batch`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub inserted: usize,
    /// Already stored (or repeated within the batch).
    pub duplicates: usize,
    /// Failed verification or belong to another room; skipped.
    pub rejected: usize,
}

/// Size caps for one sync response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCaps {
    pub max_events: usize,
    /// Sum of event wire sizes. A single event larger than this is still sent
    /// alone, so progress is always possible.
    pub max_bytes: usize,
}

impl SyncCaps {
    pub const DEFAULT: Self = Self {
        max_events: 2000,
        max_bytes: 8 << 20,
    };
}

impl Default for SyncCaps {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Current wall clock in unix millis (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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

type RawRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    String,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Option<Vec<u8>>,
);

/// A stored event plus its local arrival time.
struct Stored {
    ev: Event,
    received_at: Option<u64>,
}

impl Stored {
    fn effective_ts(&self) -> u64 {
        effective_ts(self.ev.ts, self.received_at)
    }
}

/// All events with lamport > `since` (or all), in [`OrderKey`] order.
fn load_events(
    conn: &Connection,
    room: RoomId,
    since: Option<Lamport>,
) -> Result<Vec<Stored>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.lamport, e.author_node, e.agent_name, e.ts, b.body, e.sig, e.received_at
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
            r.get(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, lamport, node, name, ts, body, sig, received_at) = row?;
        let body: EventBody = postcard::from_bytes(&body)?;
        let received_at = received_at
            .map(|b| arr::<8>(b, "received_at").map(u64::from_be_bytes))
            .transpose()?;
        let ev = Event {
            id: EventId::from_bytes(arr(id, "event id")?),
            room,
            author: AgentRef::new(NodeId::from_bytes(arr(node, "author node")?), name),
            lamport: Lamport(u64::from_be_bytes(arr(lamport, "lamport")?)),
            ts: u64::from_be_bytes(arr(ts, "ts")?),
            body,
            sig: EventSig::from_bytes(arr(sig, "sig")?),
        };
        out.push(Stored { ev, received_at });
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
    for s in load_events(conn, room, None)? {
        views.apply(&s.ev, s.effective_ts(), policy);
        last = Some(order_key(&s.ev));
    }
    Ok((views, last))
}

/// Views after adding `new` (sorted by order key, already inserted in
/// `conn`): incremental if all of `new` sorts after `last`, else full rebuild.
fn next_views(
    conn: &Connection,
    room: RoomId,
    policy: ClaimPolicy,
    base: &Views,
    last: Option<OrderKey>,
    new: &[Stored],
) -> Result<(Views, Option<OrderKey>), StoreError> {
    let in_order = match (new.first(), last) {
        (Some(first), Some(last)) => order_key(&first.ev) > last,
        _ => true,
    };
    if !in_order {
        return rebuild(conn, room, policy);
    }
    let mut views = base.clone();
    let mut last = last;
    for s in new {
        views.apply(&s.ev, s.effective_ts(), policy);
        last = Some(order_key(&s.ev));
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

/// Stores created before `received_at` existed get the column added (NULL).
fn migrate_received_at(conn: &Connection) -> Result<(), StoreError> {
    let has: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('events') WHERE name = 'received_at'",
        [],
        |r| r.get(0),
    )?;
    if !has {
        conn.execute("ALTER TABLE events ADD COLUMN received_at BLOB", [])?;
    }
    Ok(())
}

/// Insert one already-verified event. Returns false if its id was stored.
fn insert_event(conn: &Connection, e: &Event, received_at: u64) -> Result<bool, StoreError> {
    let body = postcard::to_allocvec(&e.body)?;
    let hash = blake3::hash(&body);
    conn.execute(
        "INSERT OR IGNORE INTO blobs (hash, body) VALUES (?1, ?2)",
        params![hash.as_bytes().to_vec(), body],
    )?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO events
             (id, lamport, author_node, agent_name, ts, body_hash, sig, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            e.id.as_bytes().to_vec(),
            be(e.lamport.0).to_vec(),
            e.author.node.as_bytes().to_vec(),
            e.author.agent_name,
            be(e.ts).to_vec(),
            hash.as_bytes().to_vec(),
            e.sig.as_bytes().to_vec(),
            be(received_at).to_vec(),
        ],
    )?;
    Ok(n > 0)
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
        migrate_received_at(&conn)?;
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

    /// Verify and append, received now. Idempotent on duplicate id. Views
    /// update incrementally when the event sorts after everything seen,
    /// otherwise by full rebuild.
    pub fn append(&mut self, e: &Event) -> Result<AppendOutcome, StoreError> {
        self.append_at(e, now_ms())
    }

    /// [`Store::append`] with an explicit local arrival time (unix millis).
    pub fn append_at(&mut self, e: &Event, received_at: u64) -> Result<AppendOutcome, StoreError> {
        if e.room != self.room {
            return Err(StoreError::WrongRoom);
        }
        e.verify()?;
        let tx = self.conn.transaction()?;
        if !insert_event(&tx, e, received_at)? {
            return Ok(AppendOutcome::Duplicate); // tx drops → rollback (no-op)
        }
        let new = [Stored {
            ev: e.clone(),
            received_at: Some(received_at),
        }];
        let (views, last_key) = next_views(
            &tx,
            self.room,
            self.policy,
            &self.views,
            self.last_key,
            &new,
        )?;
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(AppendOutcome::Inserted)
    }

    /// Append many events received now. See [`Store::append_batch_at`].
    pub fn append_batch(&mut self, events: &[Event]) -> Result<BatchReport, StoreError> {
        self.append_batch_at(events, now_ms())
    }

    /// Verify each event and append the valid ones in one transaction. Invalid
    /// or wrong-room events are skipped and counted, not fatal. Views are
    /// updated once: incrementally if every new event sorts after everything
    /// seen, else by a single full rebuild.
    pub fn append_batch_at(
        &mut self,
        events: &[Event],
        received_at: u64,
    ) -> Result<BatchReport, StoreError> {
        let mut report = BatchReport::default();
        let mut new = Vec::new();
        let tx = self.conn.transaction()?;
        for e in events {
            if e.room != self.room || e.verify().is_err() {
                report.rejected += 1;
                continue;
            }
            if insert_event(&tx, e, received_at)? {
                report.inserted += 1;
                new.push(Stored {
                    ev: e.clone(),
                    received_at: Some(received_at),
                });
            } else {
                report.duplicates += 1;
            }
        }
        if new.is_empty() {
            return Ok(report);
        }
        new.sort_by_key(|s| order_key(&s.ev));
        let (views, last_key) = next_views(
            &tx,
            self.room,
            self.policy,
            &self.views,
            self.last_key,
            &new,
        )?;
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(report)
    }

    /// Settle state of a lease claim at `now`, with every event's time clamped
    /// to `min(ts, received_at)` so a future-dated claim can't extend a lease.
    pub fn claim_state(
        &self,
        claim_id: &EventId,
        now: u64,
    ) -> Result<Option<ClaimState>, StoreError> {
        let events: Vec<Event> = load_events(&self.conn, self.room, None)?
            .into_iter()
            .map(|s| {
                let ts = s.effective_ts();
                Event { ts, ..s.ev }
            })
            .collect();
        Ok(settle::claim_state(claim_id, &events, now))
    }

    /// Anti-entropy summary: highest lamport stored per author node.
    pub fn have(&self) -> Result<BTreeMap<NodeId, Lamport>, StoreError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT author_node, MAX(lamport) FROM events GROUP BY author_node")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (node, lamport) = row?;
            out.insert(
                NodeId::from_bytes(arr(node, "author node")?),
                Lamport(u64::from_be_bytes(arr(lamport, "lamport")?)),
            );
        }
        Ok(out)
    }

    /// Wire bytes of events the requester lacks according to its `have`
    /// vector, in [`OrderKey`] order, capped by `caps`. Returns `more = true`
    /// if the cap cut the answer short; the requester asks again with its
    /// updated vector. A cut never separates events with the same
    /// (author node, lamport), which the vector could not tell apart.
    pub fn missing_events(
        &self,
        have: &BTreeMap<NodeId, Lamport>,
        caps: SyncCaps,
    ) -> Result<(Vec<Vec<u8>>, bool), StoreError> {
        let wanted = load_events(&self.conn, self.room, None)?
            .into_iter()
            .map(|s| s.ev)
            .filter(|e| have.get(&e.author.node).is_none_or(|h| e.lamport > *h));
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut keys: Vec<(NodeId, Lamport)> = Vec::new();
        let mut bytes = 0usize;
        for e in wanted {
            let wire = e.encode()?;
            let full = out.len() >= caps.max_events
                || (!out.is_empty() && bytes.saturating_add(wire.len()) > caps.max_bytes);
            if full {
                let next = (e.author.node, e.lamport);
                // Drop the trailing run that shares `next`'s (node, lamport),
                // unless that would leave nothing to send.
                if let Some(i) = keys.iter().rposition(|k| *k != next) {
                    out.truncate(i + 1);
                }
                return Ok((out, true));
            }
            bytes = bytes.saturating_add(wire.len());
            keys.push((e.author.node, e.lamport));
            out.push(wire);
        }
        Ok((out, false))
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
        Ok(load_events(&self.conn, self.room, Some(since))?
            .into_iter()
            .map(|s| s.ev)
            .collect())
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
