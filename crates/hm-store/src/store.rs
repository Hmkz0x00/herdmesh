//! SQLite-backed append-only event log for one room.
//!
//! Tables: `events` (keyed by EventId, unique per (author node, seq)),
//! `blobs` (bodies deduped by blake3), `authors` (contiguous seq prefix per
//! author), `equivocations` (misbehaviour marks), `first_seen` (local arrival
//! time per event, never read by views), `cursors` (local per-agent read
//! cursors), `meta` (room, pinned genesis, view snapshot).
//! Integers that take part in ordering are stored as big-endian BLOBs so
//! SQLite's memcmp order equals numeric order for the full u64 range.
//!
//! Ingestion (ADR-002 §1-3), for every append path:
//! 1. wrong room / bad signature / seq 0 / encoded size above
//!    `MAX_EVENT_BYTES` → rejected;
//! 2. `ts > now + MAX_FUTURE_SKEW` → not stored, parked in a bounded
//!    in-memory pending queue that every later append retries;
//! 3. `lamport > local max + MAX_LAMPORT_JUMP` → rejected;
//! 4. same (author, seq) with a different id → equivocation: the lowest id
//!    is kept, a mark is recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hm_proto::limits::{MAX_EVENT_BYTES, MAX_FUTURE_SKEW_MS, MAX_LAMPORT_JUMP};
use hm_proto::{AgentRef, Event, EventBody, EventId, EventSig, Lamport, NodeId, RoomId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreError;
use crate::settle::{ClaimState, SETTLE_MS};
use crate::views::{OrderKey, Views, order_key};

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
    author_node BLOB NOT NULL,
    seq         BLOB NOT NULL,
    lamport     BLOB NOT NULL,
    agent_name  TEXT NOT NULL,
    ts          BLOB NOT NULL,
    body_hash   BLOB NOT NULL REFERENCES blobs(hash),
    sig         BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS events_order ON events (lamport, author_node, id);
CREATE UNIQUE INDEX IF NOT EXISTS events_author_seq ON events (author_node, seq);
CREATE TABLE IF NOT EXISTS authors (
    node BLOB PRIMARY KEY,
    hi   BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS equivocations (
    node    BLOB NOT NULL,
    seq     BLOB NOT NULL,
    dropped BLOB NOT NULL,
    PRIMARY KEY (node, seq, dropped)
);
CREATE TABLE IF NOT EXISTS first_seen (
    id BLOB PRIMARY KEY,
    at BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS cursors (
    node    BLOB NOT NULL,
    name    TEXT NOT NULL,
    lamport BLOB NOT NULL,
    PRIMARY KEY (node, name)
);
";

const SELECT_EVENT: &str = "SELECT e.id, e.author_node, e.seq, e.lamport, e.agent_name, e.ts, \
     b.body, e.sig FROM events e JOIN blobs b ON b.hash = e.body_hash";

/// Bumped when the view snapshot layout changes; a mismatch triggers a rebuild.
const VIEW_SNAPSHOT_VERSION: u8 = 3;

/// Max events parked because their `ts` is too far in the future.
pub const PENDING_CAP: usize = 1000;

/// Most stored seqs above an author's prefix that one summary reads to list
/// holes (GATE-2 N4). Holes past it are listed once earlier ones fill.
pub const SUMMARY_TAIL_ROWS: usize = 1024;

/// Result of [`Store::append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Inserted,
    /// Same event already stored (or an equivocating twin with a lower id
    /// is stored); nothing changed.
    Duplicate,
    /// `ts` too far in the future: parked in the pending queue, not stored.
    Deferred,
}

/// Result of [`Store::append_batch`]. Counts include pending events retried
/// by this call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub inserted: usize,
    /// Already stored (or repeated within the batch).
    pub duplicates: usize,
    /// Failed verification, belong to another room, or jump the Lamport
    /// clock too far; skipped.
    pub rejected: usize,
    /// Parked in the pending queue (future `ts`).
    pub deferred: usize,
    /// Same (author, seq) as a stored event with a different id.
    pub equivocations: usize,
}

/// Size caps for one sync response (legacy lamport-based sync).
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

/// What this store holds of one author's sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorSeqs {
    /// Every seq in `1..=hi` is stored.
    pub hi: u64,
    /// Highest stored seq (`>= hi`).
    pub max: u64,
    /// Missing seqs in `hi+1..max`, ascending, at most the requested count.
    pub holes: Vec<u64>,
}

/// A recorded equivocation: `node` signed two events with the same `seq`;
/// `kept` (the lowest id) is stored, `dropped` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Equivocation {
    pub node: NodeId,
    pub seq: u64,
    pub kept: EventId,
    pub dropped: EventId,
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
    pending: Vec<Event>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("room", &self.room)
            .field("last_key", &self.last_key)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

/// What ingestion did with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disp {
    Inserted,
    Duplicate,
    Deferred,
    Rejected,
    LamportJump,
    /// Equivocation; this event has the lower id and replaced the stored one.
    Replaced,
    /// Equivocation; the stored twin has the lower id, this one was dropped.
    Dropped,
}

fn be(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

fn arr<const N: usize>(v: Vec<u8>, what: &'static str) -> Result<[u8; N], StoreError> {
    v.try_into().map_err(|_| StoreError::Corrupt(what))
}

fn u64_of(v: Vec<u8>, what: &'static str) -> Result<u64, StoreError> {
    Ok(u64::from_be_bytes(arr(v, what)?))
}

fn encode_key(k: &OrderKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(72);
    out.extend_from_slice(&k.0.0.to_be_bytes());
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

fn row_event(r: &rusqlite::Row<'_>, room: RoomId) -> Result<Event, StoreError> {
    let body: Vec<u8> = r.get(6)?;
    let body: EventBody = postcard::from_bytes(&body)?;
    Ok(Event {
        id: EventId::from_bytes(arr(r.get(0)?, "event id")?),
        room,
        author: AgentRef::new(
            NodeId::from_bytes(arr(r.get(1)?, "author node")?),
            r.get::<_, String>(4)?,
        ),
        seq: u64_of(r.get(2)?, "seq")?,
        lamport: Lamport(u64_of(r.get(3)?, "lamport")?),
        ts: u64_of(r.get(5)?, "ts")?,
        body,
        sig: EventSig::from_bytes(arr(r.get(7)?, "sig")?),
    })
}

/// Events matching `tail` (a WHERE/ORDER/LIMIT clause over alias `e`).
fn query_events<P: rusqlite::Params>(
    conn: &Connection,
    room: RoomId,
    tail: &str,
    p: P,
) -> Result<Vec<Event>, StoreError> {
    let mut stmt = conn.prepare_cached(&format!("{SELECT_EVENT} {tail}"))?;
    let mut rows = stmt.query(p)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(row_event(r, room)?);
    }
    Ok(out)
}

const ALL_IN_ORDER: &str = "ORDER BY e.lamport, e.author_node, e.id";

fn rebuild(
    conn: &Connection,
    room: RoomId,
    genesis: Option<EventId>,
) -> Result<(Views, Option<OrderKey>), StoreError> {
    let mut views = Views::new(genesis);
    let mut last = None;
    for e in query_events(conn, room, ALL_IN_ORDER, [])? {
        views.apply(&e);
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
    match last {
        Some(k) => conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_key', ?1)",
            params![encode_key(k)],
        )?,
        None => conn.execute("DELETE FROM meta WHERE key = 'last_key'", [])?,
    };
    Ok(())
}

fn get_meta(conn: &Connection, k: &str) -> Result<Option<Vec<u8>>, StoreError> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![k], |r| {
            r.get(0)
        })
        .optional()?)
}

fn load_snapshot(conn: &Connection) -> Result<Option<(Views, Option<OrderKey>)>, StoreError> {
    let Some(snap) = get_meta(conn, "views")? else {
        return Ok(None);
    };
    let views = match snap.split_first() {
        Some((&VIEW_SNAPSHOT_VERSION, rest)) => match postcard::from_bytes(rest) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };
    let last = match get_meta(conn, "last_key")? {
        Some(k) => match decode_key(&k) {
            Some(k) => Some(k),
            None => return Ok(None),
        },
        None => None,
    };
    Ok(Some((views, last)))
}

/// Stores from before per-author seqs (wire v1) can't be migrated: their
/// events can't be re-signed.
fn check_schema(conn: &Connection) -> Result<(), StoreError> {
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('events')")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !cols.is_empty() && !cols.iter().any(|c| c == "seq") {
        return Err(StoreError::IncompatibleSchema);
    }
    Ok(())
}

fn max_lamport(conn: &Connection) -> Result<Option<u64>, StoreError> {
    let max: Option<Vec<u8>> =
        conn.query_row("SELECT MAX(lamport) FROM events", [], |r| r.get(0))?;
    max.map(|m| u64_of(m, "lamport")).transpose()
}

fn id_at(conn: &Connection, node: &NodeId, seq: u64) -> Result<Option<EventId>, StoreError> {
    let id: Option<Vec<u8>> = conn
        .prepare_cached("SELECT id FROM events WHERE author_node = ?1 AND seq = ?2")?
        .query_row(params![node.as_bytes().to_vec(), be(seq)], |r| r.get(0))
        .optional()?;
    id.map(|v| arr(v, "event id").map(EventId::from_bytes))
        .transpose()
}

/// Size of `e` in wire form (`Event::encode`).
fn encoded_len(e: &Event) -> Result<usize, StoreError> {
    Ok(e.encode()?.len())
}

/// Insert one already-verified event whose (author, seq) is free.
fn insert_event(conn: &Connection, e: &Event) -> Result<(), StoreError> {
    let body = postcard::to_allocvec(&e.body)?;
    let hash = blake3::hash(&body);
    conn.prepare_cached("INSERT OR IGNORE INTO blobs (hash, body) VALUES (?1, ?2)")?
        .execute(params![hash.as_bytes().to_vec(), body])?;
    conn.prepare_cached(
        "INSERT INTO events (id, author_node, seq, lamport, agent_name, ts, body_hash, sig)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        e.id.as_bytes().to_vec(),
        e.author.node.as_bytes().to_vec(),
        be(e.seq),
        be(e.lamport.0),
        e.author.agent_name,
        be(e.ts),
        hash.as_bytes().to_vec(),
        e.sig.as_bytes().to_vec(),
    ])?;
    Ok(())
}

fn delete_event(conn: &Connection, id: &EventId) -> Result<(), StoreError> {
    let id = id.as_bytes().to_vec();
    let hash: Option<Vec<u8>> = conn
        .query_row(
            "SELECT body_hash FROM events WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM first_seen WHERE id = ?1", params![id])?;
    if let Some(h) = hash {
        conn.execute(
            "DELETE FROM blobs WHERE hash = ?1
             AND NOT EXISTS (SELECT 1 FROM events WHERE body_hash = ?1)",
            params![h],
        )?;
    }
    Ok(())
}

/// Move `node`'s contiguous prefix forward over newly stored seqs.
fn advance_hi(conn: &Connection, node: &NodeId) -> Result<(), StoreError> {
    let nb = node.as_bytes().to_vec();
    let hi: Option<Vec<u8>> = conn
        .query_row("SELECT hi FROM authors WHERE node = ?1", params![nb], |r| {
            r.get(0)
        })
        .optional()?;
    let mut hi = hi.map(|v| u64_of(v, "hi")).transpose()?.unwrap_or(0);
    while let Some(next) = hi.checked_add(1) {
        if id_at(conn, node, next)?.is_none() {
            break;
        }
        hi = next;
    }
    conn.execute(
        "INSERT OR REPLACE INTO authors (node, hi) VALUES (?1, ?2)",
        params![nb, be(hi)],
    )?;
    Ok(())
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
        check_schema(&conn)?;
        conn.execute_batch(SCHEMA)?;
        match get_meta(&conn, "room")? {
            Some(r) if r.as_slice() != room.as_bytes() => return Err(StoreError::WrongRoom),
            Some(_) => {}
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('room', ?1)",
                    params![room.as_bytes().to_vec()],
                )?;
            }
        }
        let genesis = get_meta(&conn, "genesis")?
            .map(|v| arr(v, "genesis").map(EventId::from_bytes))
            .transpose()?;
        let (views, last_key) = match load_snapshot(&conn)? {
            Some((v, last)) if v.genesis == genesis => (v, last),
            _ => rebuild(&conn, room, genesis)?,
        };
        Ok(Self {
            conn,
            room,
            views,
            last_key,
            pending: Vec::new(),
        })
    }

    pub fn room(&self) -> RoomId {
        self.room
    }

    /// Current materialized views.
    pub fn views(&self) -> &Views {
        &self.views
    }

    /// The pinned Genesis event id, if any.
    pub fn genesis(&self) -> Option<EventId> {
        self.views.genesis
    }

    /// Pin the room's Genesis (from the ticket, or our own when creating the
    /// room). Roles are anchored to it. Re-pinning the same id is a no-op; a
    /// different id is an error.
    pub fn pin_genesis(&mut self, id: EventId) -> Result<(), StoreError> {
        match self.views.genesis {
            Some(g) if g == id => return Ok(()),
            Some(_) => return Err(StoreError::GenesisMismatch),
            None => {}
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('genesis', ?1)",
            params![id.as_bytes().to_vec()],
        )?;
        let (views, last_key) = rebuild(&tx, self.room, Some(id))?;
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(())
    }

    /// Verify and append at the current time. See [`Store::append_at`].
    pub fn append(&mut self, e: &Event) -> Result<AppendOutcome, StoreError> {
        self.append_at(e, now_ms())
    }

    /// Verify and append with `now` as the local clock (unix millis), which
    /// drives the skew guard and the first-seen record. Idempotent. Also
    /// retries pending events whose time has come.
    pub fn append_at(&mut self, e: &Event, now: u64) -> Result<AppendOutcome, StoreError> {
        if e.room != self.room {
            return Err(StoreError::WrongRoom);
        }
        e.verify()?;
        let size = encoded_len(e)?;
        if size > MAX_EVENT_BYTES {
            return Err(StoreError::Oversized(size));
        }
        match self.ingest(vec![e.clone()], now)?.first() {
            Some(Disp::Inserted | Disp::Replaced) => Ok(AppendOutcome::Inserted),
            Some(Disp::Duplicate | Disp::Dropped) => Ok(AppendOutcome::Duplicate),
            Some(Disp::Deferred) => Ok(AppendOutcome::Deferred),
            Some(Disp::LamportJump) => Err(StoreError::LamportJump),
            Some(Disp::Rejected) | None => Err(StoreError::Corrupt("ingest disposition")),
        }
    }

    /// Append many events at the current time. See [`Store::append_batch_at`].
    pub fn append_batch(&mut self, events: &[Event]) -> Result<BatchReport, StoreError> {
        self.append_batch_at(events, now_ms())
    }

    /// Verify each event and append the valid ones in one transaction.
    /// Invalid events are skipped and counted, not fatal. Views are updated
    /// once: incrementally if every new event sorts after everything seen,
    /// else by a single full rebuild.
    pub fn append_batch_at(
        &mut self,
        events: &[Event],
        now: u64,
    ) -> Result<BatchReport, StoreError> {
        let disp = self.ingest(events.to_vec(), now)?;
        let mut r = BatchReport::default();
        for d in disp {
            match d {
                Disp::Inserted => r.inserted += 1,
                Disp::Duplicate => r.duplicates += 1,
                Disp::Deferred => r.deferred += 1,
                Disp::Rejected | Disp::LamportJump => r.rejected += 1,
                Disp::Replaced => {
                    r.inserted += 1;
                    r.equivocations += 1;
                }
                Disp::Dropped => r.equivocations += 1,
            }
        }
        Ok(r)
    }

    /// Retry pending (future-dated) events against `now`.
    pub fn retry_pending(&mut self, now: u64) -> Result<BatchReport, StoreError> {
        self.append_batch_at(&[], now)
    }

    /// Number of events parked in the pending queue.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Park a future-dated event. Dedupes by id; when full, keeps the
    /// events nearest in time.
    fn defer(&mut self, e: Event) {
        if self.pending.iter().any(|p| p.id == e.id) {
            return;
        }
        if self.pending.len() >= PENDING_CAP {
            let far = self
                .pending
                .iter()
                .enumerate()
                .max_by_key(|(_, p)| (p.ts, p.id))
                .map(|(i, p)| (i, (p.ts, p.id)));
            match far {
                Some((i, k)) if (e.ts, e.id) < k => {
                    self.pending.swap_remove(i);
                }
                _ => return,
            }
        }
        self.pending.push(e);
    }

    /// Core ingestion; returns one disposition per input event, then one per
    /// retried pending event.
    fn ingest(&mut self, input: Vec<Event>, now: u64) -> Result<Vec<Disp>, StoreError> {
        let horizon = now.saturating_add(MAX_FUTURE_SKEW_MS);
        let fresh = input.len();
        let (ready, wait): (Vec<Event>, Vec<Event>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|e| e.ts <= horizon);
        self.pending = wait;
        let mut all = input;
        all.extend(ready);

        let mut disp = vec![Disp::Rejected; all.len()];
        let mut todo = Vec::with_capacity(all.len());
        for (i, e) in all.iter().enumerate() {
            // Pending events were verified when first offered.
            if i < fresh
                && (e.room != self.room
                    || e.verify().is_err()
                    || encoded_len(e).map_or(true, |n| n > MAX_EVENT_BYTES))
            {
                continue;
            }
            if e.ts > horizon {
                self.defer(e.clone());
                disp[i] = Disp::Deferred;
                continue;
            }
            todo.push(i);
        }
        todo.sort_by_key(|&i| order_key(&all[i]));

        let tx = self.conn.transaction()?;
        let mut max = max_lamport(&tx)?.unwrap_or(0);
        let mut added: Vec<usize> = Vec::new();
        let mut replaced = false;
        let mut touched = BTreeSet::new();
        for i in todo {
            let e = &all[i];
            if e.lamport.0 > max.saturating_add(MAX_LAMPORT_JUMP) {
                disp[i] = Disp::LamportJump;
                continue;
            }
            match id_at(&tx, &e.author.node, e.seq)? {
                Some(id) if id == e.id => {
                    disp[i] = Disp::Duplicate;
                    continue;
                }
                Some(old) => {
                    // Mark the higher id of the pair: over any arrival order
                    // the marked set is "every offered id but the lowest".
                    tx.execute(
                        "INSERT OR IGNORE INTO equivocations (node, seq, dropped)
                         VALUES (?1, ?2, ?3)",
                        params![
                            e.author.node.as_bytes().to_vec(),
                            be(e.seq),
                            e.id.max(old).as_bytes().to_vec()
                        ],
                    )?;
                    if e.id > old {
                        disp[i] = Disp::Dropped;
                        continue;
                    }
                    delete_event(&tx, &old)?;
                    insert_event(&tx, e)?;
                    replaced = true;
                    disp[i] = Disp::Replaced;
                }
                None => {
                    insert_event(&tx, e)?;
                    disp[i] = Disp::Inserted;
                    added.push(i);
                }
            }
            tx.prepare_cached("INSERT OR IGNORE INTO first_seen (id, at) VALUES (?1, ?2)")?
                .execute(params![e.id.as_bytes().to_vec(), be(now)])?;
            max = max.max(e.lamport.0);
            touched.insert(e.author.node);
        }
        if added.is_empty() && !replaced {
            // Still commit: equivocation marks may have been recorded.
            tx.commit()?;
            return Ok(disp);
        }
        for node in &touched {
            advance_hi(&tx, node)?;
        }
        let in_order = match (added.first(), self.last_key) {
            (Some(&first), Some(last)) => order_key(&all[first]) > last,
            _ => true,
        };
        let (views, last_key) = if replaced || !in_order {
            rebuild(&tx, self.room, self.views.genesis)?
        } else {
            let mut views = self.views.clone();
            let mut last = self.last_key;
            for &i in &added {
                views.apply(&all[i]);
                last = Some(order_key(&all[i]));
            }
            (views, last)
        };
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(disp)
    }

    /// Lease state of `claim_id` at `now` (pure over the signed event set).
    pub fn claim_state(
        &self,
        claim_id: &EventId,
        now: u64,
    ) -> Result<Option<ClaimState>, StoreError> {
        Ok(self.views.claim_state(claim_id, now))
    }

    /// When this node first stored event `id` (local clock, unix millis).
    pub fn first_seen(&self, id: &EventId) -> Result<Option<u64>, StoreError> {
        let v: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT at FROM first_seen WHERE id = ?1")?
            .query_row(params![id.as_bytes().to_vec()], |r| r.get(0))
            .optional()?;
        v.map(|v| u64_of(v, "first_seen")).transpose()
    }

    /// Local settle safety (ADR-002 §1): true only if the claim is the
    /// deterministic winner and settled ([`ClaimState::Granted`]), this node
    /// has known it for at least [`SETTLE_MS`], and no conflicting live claim
    /// was first seen here within the last [`SETTLE_MS`].
    pub fn claim_safe(&self, claim_id: &EventId, now: u64) -> Result<bool, StoreError> {
        if self.views.claim_state(claim_id, now) != Some(ClaimState::Granted) {
            return Ok(false);
        }
        let recent = |seen: u64| now < seen.saturating_add(SETTLE_MS);
        match self.first_seen(claim_id)? {
            Some(seen) if !recent(seen) => {}
            _ => return Ok(false),
        }
        for other in self.views.leases.conflicting(claim_id, now) {
            if self.first_seen(&other)?.is_some_and(recent) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Per author: contiguous prefix, highest seq, and up to `max_holes`
    /// missing seqs above the prefix. See [`Store::seq_summary_page`]; this
    /// covers every author, so it is for tests and small rooms.
    pub fn seq_summary(
        &self,
        max_holes: usize,
    ) -> Result<BTreeMap<NodeId, AuthorSeqs>, StoreError> {
        self.seq_summary_page(None, usize::MAX, max_holes)
    }

    /// [`Store::seq_summary`] for at most `max_authors` authors after
    /// `after`, in node order. Bounded work per author: one prefix lookup,
    /// one indexed `MAX(seq)`, and at most [`SUMMARY_TAIL_ROWS`] tail seqs
    /// (none when `max_holes == 0`). Never reads the whole log.
    pub fn seq_summary_page(
        &self,
        after: Option<NodeId>,
        max_authors: usize,
        max_holes: usize,
    ) -> Result<BTreeMap<NodeId, AuthorSeqs>, StoreError> {
        let after = after.map_or_else(Vec::new, |n| n.as_bytes().to_vec());
        let limit = i64::try_from(max_authors).unwrap_or(i64::MAX);
        let authors: Vec<(Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT node, hi FROM authors WHERE node > ?1 ORDER BY node LIMIT ?2")?
            .query_map(params![after, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut top = self
            .conn
            .prepare_cached("SELECT MAX(seq) FROM events WHERE author_node = ?1")?;
        let mut tail = self.conn.prepare_cached(
            "SELECT seq FROM events WHERE author_node = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = i64::try_from(SUMMARY_TAIL_ROWS).unwrap_or(i64::MAX);
        let mut out = BTreeMap::new();
        for (node, hi) in authors {
            let hi = u64_of(hi, "hi")?;
            let max: Option<Vec<u8>> = top.query_row(params![node], |r| r.get(0))?;
            let max = max
                .map(|m| u64_of(m, "seq"))
                .transpose()?
                .unwrap_or(hi)
                .max(hi);
            let mut s = AuthorSeqs {
                hi,
                max,
                holes: Vec::new(),
            };
            if max_holes > 0 && max > hi {
                let seqs: Vec<Vec<u8>> = tail
                    .query_map(params![node, be(hi), rows], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                let mut last = hi;
                for seq in seqs {
                    let seq = u64_of(seq, "seq")?;
                    let mut gap = last.saturating_add(1);
                    while gap < seq && s.holes.len() < max_holes {
                        s.holes.push(gap);
                        gap += 1;
                    }
                    last = seq;
                }
            }
            out.insert(NodeId::from_bytes(arr(node, "author node")?), s);
        }
        Ok(out)
    }

    /// Events by `author` with `seq` in `holes` or `seq > after_seq`, by
    /// ascending seq (holes first), at most `limit`. Indexed point and range
    /// lookups only.
    pub fn events_for(
        &self,
        author: &NodeId,
        after_seq: u64,
        holes: &[u64],
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let node = author.as_bytes().to_vec();
        let mut wanted: Vec<u64> = holes
            .iter()
            .copied()
            .filter(|&h| h >= 1 && h <= after_seq)
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let mut out = Vec::new();
        for h in wanted {
            if out.len() >= limit {
                return Ok(out);
            }
            out.extend(query_events(
                &self.conn,
                self.room,
                "WHERE e.author_node = ?1 AND e.seq = ?2",
                params![node, be(h)],
            )?);
        }
        let left = i64::try_from(limit - out.len()).unwrap_or(i64::MAX);
        out.extend(query_events(
            &self.conn,
            self.room,
            "WHERE e.author_node = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
            params![node, be(after_seq), left],
        )?);
        Ok(out)
    }

    /// Stored events of `author` at seqs with a recorded equivocation (the
    /// kept, lowest-id twin of each), by ascending seq, at most `limit`.
    /// Served by sync so peers holding a higher twin converge (ADR-002
    /// addendum). Indexed lookups on both tables' (node, seq) keys.
    pub fn twins_for(&self, author: &NodeId, limit: usize) -> Result<Vec<Event>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        query_events(
            &self.conn,
            self.room,
            "WHERE e.author_node = ?1 AND e.seq IN              (SELECT seq FROM equivocations WHERE node = ?1) ORDER BY e.seq LIMIT ?2",
            params![author.as_bytes().to_vec(), limit],
        )
    }

    /// Recorded equivocations, by (node, seq).
    pub fn equivocations(&self) -> Result<Vec<Equivocation>, StoreError> {
        let rows: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = self
            .conn
            .prepare_cached(
                "SELECT node, seq, dropped FROM equivocations ORDER BY node, seq, dropped",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(n, s, d)| {
                let node = NodeId::from_bytes(arr(n, "node")?);
                let seq = u64_of(s, "seq")?;
                Ok(Equivocation {
                    node,
                    seq,
                    kept: id_at(&self.conn, &node, seq)?.ok_or(StoreError::Corrupt("kept"))?,
                    dropped: EventId::from_bytes(arr(d, "dropped")?),
                })
            })
            .collect()
    }

    /// Seq for the next event authored by `node`: highest stored + 1.
    pub fn next_seq(&self, node: &NodeId) -> Result<u64, StoreError> {
        let max: Option<Vec<u8>> = self.conn.query_row(
            "SELECT MAX(seq) FROM events WHERE author_node = ?1",
            params![node.as_bytes().to_vec()],
            |r| r.get(0),
        )?;
        let max = max.map(|m| u64_of(m, "seq")).transpose()?.unwrap_or(0);
        max.checked_add(1).ok_or(StoreError::ClockExhausted)
    }

    /// Lamport for the next locally authored event: `max(seen) + 1`.
    pub fn next_lamport(&self) -> Result<Lamport, StoreError> {
        crate::views::next_lamport(max_lamport(&self.conn)?.map(Lamport))
            .ok_or(StoreError::ClockExhausted)
    }

    /// Legacy lamport sync: highest lamport stored per author node.
    /// Superseded by [`Store::seq_summary`]; kept until hm-net migrates.
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
                Lamport(u64_of(lamport, "lamport")?),
            );
        }
        Ok(out)
    }

    /// Legacy lamport sync (full scan): wire bytes of events the requester
    /// lacks per its `have` vector, capped by `caps`; `more` if cut short.
    /// Superseded by [`Store::events_for`]; kept until hm-net migrates.
    pub fn missing_events(
        &self,
        have: &BTreeMap<NodeId, Lamport>,
        caps: SyncCaps,
    ) -> Result<(Vec<Vec<u8>>, bool), StoreError> {
        let wanted = query_events(&self.conn, self.room, ALL_IN_ORDER, [])?
            .into_iter()
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
        let (views, last_key) = rebuild(&tx, self.room, self.views.genesis)?;
        save_views(&tx, &views, last_key.as_ref())?;
        tx.commit()?;
        self.views = views;
        self.last_key = last_key;
        Ok(())
    }

    /// Events with lamport strictly greater than `since`, in order.
    pub fn events_since(&self, since: Lamport) -> Result<Vec<Event>, StoreError> {
        query_events(
            &self.conn,
            self.room,
            "WHERE e.lamport > ?1 ORDER BY e.lamport, e.author_node, e.id",
            params![be(since.0)],
        )
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
        v.map(|b| u64_of(b, "cursor").map(Lamport)).transpose()
    }

    /// Move `agent`'s cursor forward to `to`. Never moves it back.
    pub fn advance_cursor(&mut self, agent: &AgentRef, to: Lamport) -> Result<(), StoreError> {
        if self.cursor(agent)?.is_some_and(|cur| cur >= to) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO cursors (node, name, lamport) VALUES (?1, ?2, ?3)",
            params![agent.node.as_bytes().to_vec(), agent.agent_name, be(to.0)],
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sync queries must be indexed lookups, never a scan of `events`.
    #[test]
    fn sync_queries_use_the_author_seq_index() {
        let s = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
        for sql in [
            format!("{SELECT_EVENT} WHERE e.author_node = ?1 AND e.seq = ?2"),
            format!(
                "{SELECT_EVENT} WHERE e.author_node = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3"
            ),
            "SELECT seq FROM events WHERE author_node = ?1 AND seq > ?2 ORDER BY seq".into(),
            "SELECT MAX(seq) FROM events WHERE author_node = ?1".into(),
        ] {
            let mut stmt = s
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            // Unbound parameters are NULL; fine for planning.
            let mut rows = stmt.raw_query();
            let mut plan: Vec<String> = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                plan.push(r.get(3).unwrap());
            }
            let events: Vec<&String> = plan
                .iter()
                .filter(|p| p.contains(" e ") || p.contains("events"))
                .collect();
            assert!(
                events.iter().all(|p| p.contains("events_author_seq")),
                "{sql}: {plan:?}"
            );
        }
    }
}
