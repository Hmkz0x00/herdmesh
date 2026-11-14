//! Daemon core: rooms, identity, and the implementation of every [`Call`].
//!
//! The daemon owns the node key; agents are named by the request tag. Every
//! write is a signed event with the next seq and lamport, checked against
//! the author's role first, then handed to the [`NetHandle`] for gossip.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ed25519_dalek::SigningKey;
use hm_brief::{agent_label, clean};
use hm_net::SharedStore;
use hm_proto::budget::{BRIEF_DEFAULT_MAX, ROOM_CARD_MAX, estimate_tokens};
use hm_proto::{
    AgentRef, Event, EventBody, EventId, Lamport, NodeId, Ref, Role, RoomId, TaskId, TaskStatus,
};
use hm_store::{AppendOutcome, ClaimState, Store, now_ms, settle::paths_overlap};

use crate::ipc::{Call, ErrCode, Handler, Items, Request, Response};

/// Author label for daemon-generated text (cards, briefs, acknowledgements).
pub const DAEMON_AUTHOR: &str = "herdmesh";
const MAX_BRIEF_TOKENS: usize = 4000;
const INBOX_MAX: usize = 50;
const SEARCH_WINDOW: u64 = 1000;
const SEARCH_MAX: usize = 100;
const TEXT_CHARS: usize = 1000;
/// Most events one author contributes to one [`Daemon::pending_pings`] scan.
pub const PING_SCAN_MAX: usize = 128;

/// Outbound side of the network, so tests run without sockets.
pub trait NetHandle: Send + Sync + 'static {
    /// Called once for every locally authored event, after it is stored.
    fn publish(&self, room: RoomId, event: &Event);
}

/// Drops everything; for tests and offline use.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNet;

impl NetHandle for NullNet {
    fn publish(&self, _: RoomId, _: &Event) {}
}

/// How the MCP layer wraps text: used to fit cards and briefs *after*
/// wrapping. `hm-mcp` has a test that keeps this in lockstep.
pub fn wrapped_cost(author: &str, text: &str) -> usize {
    fn esc(s: &str) -> String {
        let mut o = String::new();
        for c in s.chars() {
            match c {
                '&' => o.push_str("&amp;"),
                '<' => o.push_str("&lt;"),
                '>' => o.push_str("&gt;"),
                '"' => o.push_str("&quot;"),
                c if c.is_control() && c != '\n' && c != '\t' => {
                    o.push_str(&format!("\\u{{{:x}}}", c as u32));
                }
                c => o.push(c),
            }
        }
        o
    }
    estimate_tokens(&format!(
        "<<peer-untrusted from=\"{}\">>{}<</peer-untrusted>>",
        esc(author),
        esc(text)
    ))
}

/// One room as held by the daemon. The store is shared with the net layer
/// (sync serving, gossip intake); lock order is always rooms, then store.
struct RoomEntry {
    name: String,
    store: SharedStore,
}

/// A locked room, as the request handlers see it.
struct RoomState<'a> {
    name: &'a str,
    store: &'a mut Store,
}

/// Something a local agent should be told about (see [`Daemon::pending_pings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    pub room: RoomId,
    /// The Ask/Handoff event that triggered it.
    pub event: EventId,
    /// Local agent name the event is addressed to.
    pub agent: String,
    /// `"Ask"` or `"Handoff"`.
    pub kind: &'static str,
    /// Sanitised `node6/name` label of the sender.
    pub from: String,
    /// Task id of a handoff (e.g. `task:t9`), if any. Never free text.
    pub re: Option<String>,
}

struct Rooms {
    by_id: BTreeMap<RoomId, RoomEntry>,
    default: Option<RoomId>,
}

/// The per-machine daemon core.
pub struct Daemon {
    key: SigningKey,
    node: NodeId,
    net: Arc<dyn NetHandle>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    rooms: Mutex<Rooms>,
    redactions: AtomicU64,
    ping_scanned: AtomicU64,
    /// Ping scan position per (room, author node); see `pending_pings`.
    ping_pos: Mutex<HashMap<(RoomId, NodeId), ScanPos>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type Res<T> = Result<T, ErrCode>;

fn internal<E>(_: E) -> ErrCode {
    ErrCode::Internal
}

fn ok(text: String) -> Res<Items> {
    Ok(vec![(DAEMON_AUTHOR.to_owned(), text)])
}

/// Most missing seqs remembered per author for the ping scan.
const PING_HOLES_MAX: usize = 64;

/// How far the ping scan has read one author's log in one room.
#[derive(Debug, Default)]
struct ScanPos {
    /// Highest seq scanned.
    after: u64,
    /// Seqs below `after` that were missing when scanned past.
    holes: BTreeSet<u64>,
}

impl ScanPos {
    fn advance(&mut self, seq: u64) {
        if seq <= self.after {
            self.holes.remove(&seq);
            return;
        }
        for gap in self.after.saturating_add(1)..seq {
            if self.holes.len() >= PING_HOLES_MAX {
                break;
            }
            self.holes.insert(gap);
        }
        self.after = seq;
    }
}

/// A ping if `e` is an Ask/Handoff to a local agent, past its inbox cursor,
/// from someone else whose role allows it.
fn ping_for(
    room: RoomId,
    e: &Event,
    views: &hm_store::Views,
    inbox: &BTreeMap<&AgentRef, Lamport>,
) -> Option<Ping> {
    let (to, kind, re) = match &e.body {
        EventBody::Ask { to: Some(t), .. } => (t, "Ask", None),
        EventBody::Handoff {
            to: Some(t), refs, ..
        } => (
            t,
            "Handoff",
            // Only a task id: git refs carry free-form paths.
            refs.iter()
                .find(|r| matches!(r, Ref::Task(_)))
                .map(ToString::to_string),
        ),
        _ => return None,
    };
    let cursor = inbox.get(to)?;
    if e.author == *to || e.lamport <= *cursor || !views.role_of(&e.author.node).may(e.kind()) {
        return None;
    }
    Some(Ping {
        room,
        event: e.id,
        agent: to.agent_name.clone(),
        kind,
        from: agent_label(&e.author),
        re,
    })
}

/// Outcome of resolving an ask/handoff target.
enum Target {
    Found(AgentRef),
    /// Not sent; the text tells the caller which agents exist.
    Unknown(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Pick {
    One(AgentRef),
    None,
    Many { hits: Vec<AgentRef>, total: usize },
}

/// Maximum candidates retained or names formatted for a resolver error.
const RESOLVE_NAMES_MAX: usize = 20;

/// Built once per recipient lookup, from an unlocked membership snapshot.
struct TargetIndex<'a> {
    names: BTreeMap<&'a str, Vec<&'a AgentRef>>,
    people: HashMap<&'a str, Vec<&'a AgentRef>>,
    /// Members that passed the shared name validator (the only ones indexed).
    indexed: usize,
}

impl<'a> TargetIndex<'a> {
    fn new(agents: &'a [AgentRef]) -> Self {
        let mut index = Self {
            names: BTreeMap::new(),
            people: HashMap::new(),
            indexed: 0,
        };
        for a in agents {
            // Peers are not bound by local name limits (N17): oversized or
            // malformed names are never indexed, so no prefix work is done.
            if !valid_agent(&a.agent_name) {
                continue;
            }
            index.indexed += 1;
            index.names.entry(&a.agent_name).or_default().push(a);
            // Preserve aliases for people whose names themselves contain '-'.
            for (pos, _) in a.agent_name.match_indices('-') {
                index
                    .people
                    .entry(&a.agent_name[..pos])
                    .or_default()
                    .push(a);
            }
        }
        index
    }

    /// Hex prefixes of at least four digits are reserved for node identities.
    /// A failed node lookup must never be reinterpreted as a person alias.
    fn pick(&self, to: &str) -> Pick {
        if let Some((left, right)) = to.split_once('/') {
            if left.len() >= 4 && left.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Self::narrow(
                    self.names
                        .get(right)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|a| a.node.to_string().starts_with(left)),
                );
            }
            return Self::narrow(
                self.people
                    .get(left)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|a| a.agent_name[left.len() + 1..].starts_with(right)),
            );
        }
        if let Some(exact) = self.names.get(to) {
            return Self::narrow(exact.iter().copied());
        }
        Self::narrow(
            self.names
                .range(to..)
                .take_while(|(name, _)| name.starts_with(to))
                .flat_map(|(_, agents)| agents.iter().copied()),
        )
    }

    fn narrow(agents: impl Iterator<Item = &'a AgentRef>) -> Pick {
        let mut hits = Vec::new();
        let mut total = 0;
        for a in agents {
            total += 1;
            if hits.len() < RESOLVE_NAMES_MAX {
                hits.push(a.clone());
            }
        }
        match total {
            0 => Pick::None,
            1 => Pick::One(hits.remove(0)),
            _ => Pick::Many { hits, total },
        }
    }

    /// Format only the capped candidates; duplicate counts come from the index.
    fn names_list<'b>(&self, agents: impl Iterator<Item = &'b AgentRef>, total: usize) -> String {
        let names: Vec<_> = agents
            .take(RESOLVE_NAMES_MAX)
            .map(|a| {
                let name = clean(&a.agent_name, 40);
                if self
                    .names
                    .get(a.agent_name.as_str())
                    .is_some_and(|v| v.len() > 1)
                {
                    format!("{name}@{}", &a.node.to_string()[..6])
                } else {
                    name
                }
            })
            .collect();
        let more = total.saturating_sub(names.len());
        let mut out = names.join(", ");
        if out.is_empty() {
            out.push_str("(none yet)");
        }
        if more > 0 {
            out.push_str(&format!(", and {more} more"));
        }
        out
    }
}

#[cfg(test)]
fn pick_target(agents: &[AgentRef], to: &str) -> Pick {
    TargetIndex::new(agents).pick(to)
}

#[cfg(test)]
fn names_list(agents: &[AgentRef]) -> String {
    let index = TargetIndex::new(agents);
    index.names_list(index.names.values().flatten().copied(), index.indexed)
}

fn valid_agent(name: &str) -> bool {
    // One shared rule for every name; also rejects secret-looking names (N7).
    hm_proto::names::valid_name(name)
}

fn parse_status(s: &str) -> Res<TaskStatus> {
    Ok(match s {
        "open" => TaskStatus::Open,
        "in_progress" => TaskStatus::InProgress,
        "blocked" => TaskStatus::Blocked,
        "done" => TaskStatus::Done,
        "dropped" => TaskStatus::Dropped,
        _ => return Err(ErrCode::BadRequest),
    })
}

fn status_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Open => "open",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Dropped => "dropped",
    }
}

fn parse_refs(refs: &[String]) -> Res<Vec<Ref>> {
    refs.iter()
        .map(|r| r.parse().map_err(|_| ErrCode::BadRequest))
        .collect()
}

fn parse_event_id(s: &str) -> Res<EventId> {
    EventId::from_hex(s).ok_or(ErrCode::BadRequest)
}

/// Text of an event worth searching or showing in an inbox.
fn body_text(e: &Event) -> Option<String> {
    Some(match &e.body {
        EventBody::Ask { q, .. } => q.clone(),
        EventBody::Answer { a, .. } => a.clone(),
        EventBody::Note { text, .. } => text.clone(),
        EventBody::Decision {
            title, rationale, ..
        } => format!("{title} {rationale}"),
        EventBody::Handoff { summary, .. } => summary.clone(),
        EventBody::TaskAdd { title, .. } => title.clone(),
        EventBody::TaskUpdate { note: Some(n), .. } => n.clone(),
        EventBody::Digest { text, .. } => text.clone(),
        _ => return None,
    })
}

/// Redact secrets in every string of `body` (in place): free text, paths,
/// git ref paths, task ids and addressee names. Returns the kinds found,
/// never the secrets. Safe to run twice: markers are stable.
pub fn redact_body(body: &mut EventBody) -> Vec<hm_proto::redact::Kind> {
    let mut hits = Vec::new();
    let mut scrub = |text: &mut String| {
        let r = hm_proto::redact::redact(text);
        if !r.hits.is_empty() {
            *text = r.text;
            hits.extend(r.hits);
        }
    };
    match body {
        EventBody::Ask { to, q } => {
            scrub_to(to, &mut scrub);
            scrub(q);
        }
        EventBody::Answer { a, .. } => scrub(a),
        EventBody::Note { scope, text } => {
            scrub(scope);
            scrub(text);
        }
        EventBody::Digest { text, .. } => scrub(text),
        EventBody::Decision {
            title,
            rationale,
            refs,
        } => {
            scrub(title);
            scrub(rationale);
            scrub_refs(refs, &mut scrub);
        }
        EventBody::Handoff {
            to,
            summary,
            refs,
            next_steps,
        } => {
            scrub_to(to, &mut scrub);
            scrub(summary);
            scrub_refs(refs, &mut scrub);
            next_steps.iter_mut().for_each(&mut scrub);
        }
        EventBody::TaskAdd { task, title, paths } => {
            scrub_task(task, &mut scrub);
            scrub(title);
            paths.iter_mut().for_each(&mut scrub);
        }
        EventBody::TaskUpdate { task, note, .. } => {
            scrub_task(task, &mut scrub);
            if let Some(n) = note {
                scrub(n);
            }
        }
        EventBody::LeaseClaim { paths, .. } => paths.iter_mut().for_each(&mut scrub),
        _ => {}
    }
    hits
}

fn scrub_to(to: &mut Option<AgentRef>, scrub: &mut impl FnMut(&mut String)) {
    if let Some(t) = to {
        scrub(&mut t.agent_name);
    }
}

/// A secret-like task id becomes `redacted-<hash>`: still a valid id, and
/// the same secret maps to the same placeholder, so updates keep matching.
fn scrub_task(task: &mut TaskId, scrub: &mut impl FnMut(&mut String)) {
    let mut s = task.as_str().to_owned();
    scrub(&mut s);
    if s != task.as_str() {
        let h = blake3::hash(task.as_str().as_bytes()).to_hex();
        if let Ok(t) = TaskId::new(format!("redacted-{}", &h[..16])) {
            *task = t;
        }
    }
}

/// Git ref paths are free-form; task refs get the task-id treatment. A git
/// ref whose redacted path is no longer valid is dropped.
fn scrub_refs(refs: &mut Vec<Ref>, scrub: &mut impl FnMut(&mut String)) {
    refs.retain_mut(|r| match r {
        Ref::Git(g) => {
            let mut p = g.path().to_owned();
            scrub(&mut p);
            if p == g.path() {
                return true;
            }
            match hm_proto::GitRef::new(g.sha(), p, g.lines()) {
                Ok(n) => {
                    *g = n;
                    true
                }
                Err(_) => false,
            }
        }
        Ref::Task(t) => {
            scrub_task(t, scrub);
            true
        }
        Ref::Event(_) => true,
    });
}

impl Daemon {
    pub fn new(key: SigningKey, net: Arc<dyn NetHandle>) -> Self {
        Self::with_clock(key, net, Box::new(now_ms))
    }

    /// Like [`Daemon::new`] with an injectable clock (unix millis).
    pub fn with_clock(
        key: SigningKey,
        net: Arc<dyn NetHandle>,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let node = NodeId::from_verifying_key(&key.verifying_key());
        Self {
            key,
            node,
            net,
            clock,
            redactions: AtomicU64::new(0),
            ping_scanned: AtomicU64::new(0),
            ping_pos: Mutex::new(HashMap::new()),
            rooms: Mutex::new(Rooms {
                by_id: BTreeMap::new(),
                default: None,
            }),
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.node
    }

    /// Register a room whose genesis is already pinned in `store` (joined
    /// from a ticket). The first room added becomes the default.
    pub fn add_room(&self, name: &str, store: Store) -> RoomId {
        self.add_room_shared(name, Arc::new(Mutex::new(store)))
    }

    /// Like [`Daemon::add_room`] for a store the net layer also holds.
    pub fn add_room_shared(&self, name: &str, store: SharedStore) -> RoomId {
        let id = lock(&store).room();
        let mut rooms = lock(&self.rooms);
        rooms.default.get_or_insert(id);
        rooms.by_id.insert(
            id,
            RoomEntry {
                name: name.to_owned(),
                store,
            },
        );
        id
    }

    /// Ids of every loaded room.
    pub fn room_ids(&self) -> Vec<RoomId> {
        lock(&self.rooms).by_id.keys().copied().collect()
    }

    fn entry(&self, room: RoomId) -> Res<(String, SharedStore)> {
        lock(&self.rooms)
            .by_id
            .get(&room)
            .map(|e| (e.name.clone(), Arc::clone(&e.store)))
            .ok_or(ErrCode::NoRoom)
    }

    #[doc(hidden)]
    pub fn store_for_test(&self, room: RoomId) -> SharedStore {
        self.entry(room)
            .map(|e| e.1)
            .unwrap_or_else(|_| unreachable!("test room"))
    }

    /// Names of the agents of *this node* registered in `room`.
    pub fn local_members(&self, room: RoomId) -> Vec<String> {
        let Ok((_, store)) = self.entry(room) else {
            return Vec::new();
        };
        lock(&store)
            .views()
            .members
            .keys()
            .filter(|a| a.node == self.node)
            .map(|a| a.agent_name.clone())
            .collect()
    }

    /// Announce `status` for local agent `agent` (must be a room member).
    pub fn set_presence(
        &self,
        room: RoomId,
        agent: &str,
        status: hm_proto::AgentStatus,
    ) -> Result<(), ErrCode> {
        if !valid_agent(agent) {
            return Err(ErrCode::BadRequest);
        }
        let (name, store) = self.entry(room)?;
        let mut guard = lock(&store);
        if !guard
            .views()
            .members
            .contains_key(&AgentRef::new(self.node, agent))
        {
            return Err(ErrCode::NotFound);
        }
        let mut st = RoomState {
            name: &name,
            store: &mut guard,
        };
        self.emit(&mut st, agent, EventBody::Presence { status })
            .map(|_| ())
    }

    /// New unread Asks and Handoffs addressed to local agents, i.e. events
    /// not offered by an earlier call and past the addressee's inbox cursor.
    /// The caller keeps them until delivered.
    ///
    /// Incremental (N6): each author's log is read from a per-room seq
    /// cursor (indexed), at most [`PING_SCAN_MAX`] events per author per
    /// call; seqs skipped as missing are re-checked by point lookup.
    pub fn pending_pings(&self) -> Vec<Ping> {
        let mut out = Vec::new();
        for room in self.room_ids() {
            let Ok((_, store)) = self.entry(room) else {
                continue;
            };
            let store = lock(&store);
            let Ok(summary) = store.seq_summary(0) else {
                continue;
            };
            let views = store.views();
            let inbox: BTreeMap<&AgentRef, Lamport> = views
                .members
                .keys()
                .filter(|a| a.node == self.node)
                .map(|me| {
                    let key = AgentRef::new(me.node, format!("inbox:{}", me.agent_name));
                    let c = store.cursor(&key).ok().flatten().unwrap_or(Lamport(0));
                    (me, c)
                })
                .collect();
            let mut pos = lock(&self.ping_pos);
            for (author, seqs) in &summary {
                let p = pos.entry((room, *author)).or_default();
                if seqs.max <= p.after && p.holes.is_empty() {
                    continue;
                }
                let holes: Vec<u64> = p.holes.iter().copied().collect();
                let Ok(events) = store.events_for(author, p.after, &holes, PING_SCAN_MAX) else {
                    continue;
                };
                self.ping_scanned
                    .fetch_add(events.len() as u64, Ordering::Relaxed);
                for e in &events {
                    p.advance(e.seq);
                    if let Some(ping) = ping_for(room, e, views, &inbox) {
                        out.push(ping);
                    }
                }
            }
        }
        out
    }

    /// Start a new room owned by this node: sign Genesis, pin it, store it.
    pub fn create_room(&self, name: &str, mut store: Store) -> Result<RoomId, ErrCode> {
        let room = store.room();
        let ev = Event::sign(
            &self.key,
            room,
            "daemon",
            store.next_seq(&self.node).map_err(internal)?,
            store.next_lamport().map_err(internal)?,
            (self.clock)(),
            EventBody::Genesis { owner: self.node },
        )
        .map_err(internal)?;
        store.pin_genesis(ev.id).map_err(internal)?;
        store.append_at(&ev, (self.clock)()).map_err(internal)?;
        self.net.publish(room, &ev);
        Ok(self.add_room(name, store))
    }

    /// Secrets never leave the machine: scrub, count, warn (kinds only).
    fn scrub(&self, room: RoomId, agent: &str, body: &mut EventBody) {
        let hits = redact_body(body);
        if hits.is_empty() {
            return;
        }
        self.redactions
            .fetch_add(hits.len() as u64, Ordering::Relaxed);
        let kinds: Vec<&str> = hits.iter().map(|k| k.as_str()).collect();
        tracing::warn!(%room, agent, ?kinds, "redacted secret-like text before signing");
    }

    /// Events decoded by [`Daemon::pending_pings`] so far (cost probe).
    #[doc(hidden)]
    pub fn ping_scanned(&self) -> u64 {
        self.ping_scanned.load(Ordering::Relaxed)
    }

    /// Secret-like spans redacted from locally authored events so far.
    pub fn redaction_count(&self) -> u64 {
        self.redactions.load(Ordering::Relaxed)
    }

    fn emit(&self, st: &mut RoomState<'_>, agent: &str, mut body: EventBody) -> Res<Event> {
        self.scrub(st.store.room(), agent, &mut body);
        let now = (self.clock)();
        if !st.store.views().role_of(&self.node).may(body.kind()) {
            return Err(ErrCode::Forbidden);
        }
        let ev = Event::sign(
            &self.key,
            st.store.room(),
            agent,
            st.store.next_seq(&self.node).map_err(internal)?,
            st.store.next_lamport().map_err(internal)?,
            now,
            body,
        )
        .map_err(internal)?;
        match st.store.append_at(&ev, now).map_err(internal)? {
            AppendOutcome::Inserted => {}
            _ => return Err(ErrCode::Internal),
        }
        self.net.publish(st.store.room(), &ev);
        Ok(ev)
    }

    /// First request from an agent announces it to the room.
    fn ensure_member(&self, st: &mut RoomState<'_>, me: &AgentRef) -> Res<()> {
        if st.store.views().members.contains_key(me) {
            return Ok(());
        }
        self.emit(st, &me.agent_name, EventBody::Join { role: Role::Member })
            .map(|_| ())
    }

    /// Resolve an ask/handoff target; see [`TargetIndex`]. No match is not an
    /// error code (the MCP layer would flatten it to "unavailable"): it is a
    /// message listing the room's agents, and nothing is sent.
    fn resolve(&self, room: Option<RoomId>, agent: &str, to: &str) -> Res<Target> {
        let to = to.trim();
        let (left, right) = to.split_once('/').unwrap_or((to, ""));
        let part_ok = |p: &str| {
            !p.is_empty()
                && p.len() <= 2 * hm_proto::names::NAME_MAX
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !hm_proto::redact::contains_secret(p)
        };
        if !part_ok(left) || (to.contains('/') && !part_ok(right)) {
            return Err(ErrCode::BadRequest);
        }
        let agents = self.recipient_snapshot(room, agent)?;
        let index = TargetIndex::new(&agents);
        Ok(match index.pick(to) {
            Pick::One(a) => Target::Found(a),
            Pick::None => Target::Unknown(format!(
                "No agent matches '{}'. Agents in this room: {}. Nothing was sent.",
                clean(to, 40),
                index.names_list(index.names.values().flatten().copied(), index.indexed)
            )),
            Pick::Many { hits, total } => Target::Unknown(format!(
                "'{}' matches several agents: {}. Use person/agent or node6/name. Nothing was sent.",
                clean(to, 40),
                index.names_list(hits.iter(), total)
            )),
        })
    }

    /// Clone membership while locked; all indexing and formatting happens later.
    fn recipient_snapshot(&self, room: Option<RoomId>, agent: &str) -> Res<Vec<AgentRef>> {
        let store = {
            let rooms = lock(&self.rooms);
            let id = room.or(rooms.default).ok_or(ErrCode::NoRoom)?;
            Arc::clone(&rooms.by_id.get(&id).ok_or(ErrCode::NoRoom)?.store)
        };
        let me = AgentRef::new(self.node, agent);
        let mut agents = {
            let guard = lock(&store);
            guard.views().members.keys().cloned().collect::<Vec<_>>()
        };
        // `call` announces the sender before consuming this resolution result.
        if !agents.contains(&me) {
            agents.push(me);
        }
        Ok(agents)
    }

    fn call(
        &self,
        st: &mut RoomState<'_>,
        agent: &str,
        call: Call,
        resolved: Option<Res<Target>>,
    ) -> Res<Items> {
        let me = AgentRef::new(self.node, agent);
        self.ensure_member(st, &me)?;
        let now = (self.clock)();
        match call {
            Call::RoomCard => ok(fit_card(hm_brief::room_card(
                st.name,
                st.store.views(),
                now,
            ))),
            Call::Brief { max_tokens } => self.brief(st, &me, max_tokens as usize, now),
            Call::Inbox { ack } => Self::inbox(st, &me, ack),
            Call::Ask { question, .. } => {
                let to = match resolved.ok_or(ErrCode::Internal)?? {
                    Target::Found(a) => a,
                    Target::Unknown(msg) => return ok(msg),
                };
                let ev = self.emit(
                    st,
                    agent,
                    EventBody::Ask {
                        to: Some(to),
                        q: question,
                    },
                )?;
                ok(format!("asked id={}", ev.id))
            }
            Call::Answer { ask_id, text } => {
                let id = parse_event_id(&ask_id)?;
                let known = st
                    .store
                    .events_since(Lamport(0))
                    .map_err(internal)?
                    .into_iter()
                    .any(|e| e.id == id && matches!(e.body, EventBody::Ask { .. }));
                if !known {
                    return Err(ErrCode::NotFound);
                }
                let ev = self.emit(
                    st,
                    agent,
                    EventBody::Answer {
                        ask_id: id,
                        a: text,
                    },
                )?;
                ok(format!("answered id={}", ev.id))
            }
            Call::Handoff {
                to,
                summary,
                refs,
                next_steps,
            } => {
                let to = match if to.is_some() {
                    Some(resolved.ok_or(ErrCode::Internal)??)
                } else {
                    None
                } {
                    None => None,
                    Some(Target::Found(a)) => Some(a),
                    Some(Target::Unknown(msg)) => return ok(msg),
                };
                let refs = parse_refs(&refs)?;
                let ev = self.emit(
                    st,
                    agent,
                    EventBody::Handoff {
                        to,
                        summary,
                        refs,
                        next_steps,
                    },
                )?;
                ok(format!("handoff id={}", ev.id))
            }
            Call::Claim { paths, ttl_min } => {
                let ttl_secs = ttl_min.checked_mul(60).ok_or(ErrCode::BadRequest)?;
                let ev = self.emit(st, agent, EventBody::LeaseClaim { paths, ttl_secs })?;
                Self::claim_result(st, &ev, (self.clock)())
            }
            Call::Release { paths } => {
                let now = (self.clock)();
                let views = st.store.views();
                let claims: Vec<EventId> = views
                    .leases
                    .claims()
                    .filter(|c| c.holder == me)
                    .filter(|c| {
                        matches!(
                            views.claim_state(&c.id, now),
                            Some(ClaimState::Pending | ClaimState::Granted)
                        )
                    })
                    .filter(|c| {
                        c.paths
                            .iter()
                            .any(|a| paths.iter().any(|b| paths_overlap(a, b)))
                    })
                    .map(|c| c.id)
                    .collect();
                if claims.is_empty() {
                    return ok("nothing to release".to_owned());
                }
                for claim in &claims {
                    self.emit(st, agent, EventBody::LeaseRelease { claim: *claim })?;
                }
                ok(format!("released {}", claims.len()))
            }
            Call::TaskAdd { task, title, paths } => {
                let task = TaskId::new(task).map_err(|_| ErrCode::BadRequest)?;
                let ev = self.emit(
                    st,
                    agent,
                    EventBody::TaskAdd {
                        task: task.clone(),
                        title,
                        paths,
                    },
                )?;
                ok(format!("task {task} id={}", ev.id))
            }
            Call::TaskUpdate { task, status, note } => {
                let task = TaskId::new(task).map_err(|_| ErrCode::BadRequest)?;
                let status = parse_status(&status)?;
                if !st.store.views().board.contains_key(&task) {
                    return Err(ErrCode::NotFound);
                }
                self.emit(
                    st,
                    agent,
                    EventBody::TaskUpdate {
                        task: task.clone(),
                        status,
                        note,
                    },
                )?;
                ok(format!("task {task} {}", status_str(status)))
            }
            Call::TaskList { filter } => {
                let want = filter.as_deref().map(parse_status).transpose()?;
                Ok(st
                    .store
                    .views()
                    .board
                    .iter()
                    .filter(|(_, t)| want.is_none_or(|w| t.status == w))
                    .take(INBOX_MAX)
                    .map(|(id, t)| {
                        (
                            agent_label(&t.created_by),
                            format!("{id} {}: {}", status_str(t.status), clean(&t.title, 200)),
                        )
                    })
                    .collect())
            }
            Call::Decide {
                title,
                rationale,
                refs,
            } => {
                let refs = parse_refs(&refs)?;
                let ev = self.emit(
                    st,
                    agent,
                    EventBody::Decision {
                        title,
                        rationale,
                        refs,
                    },
                )?;
                ok(format!("decided id={}", ev.id))
            }
            Call::GetEvent { id } => {
                let id = parse_event_id(&id)?;
                match hm_brief::get_event(st.store, &id).map_err(internal)? {
                    Some(t) => ok(t),
                    None => Err(ErrCode::NotFound),
                }
            }
            Call::Search { query, limit } => Self::search(st, &query, limit as usize),
            // Served by `handle_request` without joining; never reaches here.
            Call::Feed { .. } | Call::FeedEvent { .. } => Err(ErrCode::Internal),
        }
    }

    fn claim_result(st: &RoomState<'_>, ev: &Event, now: u64) -> Res<Items> {
        let state = st.store.claim_state(&ev.id, now).map_err(internal)?;
        let text = match state {
            Some(ClaimState::Lost { winner }) => {
                let who = st.store.views().leases.claim(&winner).map_or_else(
                    || "someone".to_owned(),
                    |c| {
                        format!(
                            "{} {}",
                            agent_label(&c.holder),
                            clean(&c.paths.join(" "), 200)
                        )
                    },
                );
                format!("lost: held by {who}")
            }
            Some(ClaimState::Granted) if st.store.claim_safe(&ev.id, now).map_err(internal)? => {
                format!("granted id={}", ev.id)
            }
            Some(ClaimState::Pending | ClaimState::Granted) => {
                format!("pending id={}", ev.id)
            }
            _ => return Err(ErrCode::Internal),
        };
        ok(text)
    }

    fn brief(&self, st: &mut RoomState<'_>, me: &AgentRef, max: usize, now: u64) -> Res<Items> {
        let max = if max == 0 { BRIEF_DEFAULT_MAX } else { max }.min(MAX_BRIEF_TOKENS);
        let cursor = st.store.cursor(me).map_err(internal)?.unwrap_or(Lamport(0));
        // Fit *after* MCP wrapping: shrink the inner budget until it does.
        let mut inner = max;
        let b = loop {
            let b = hm_brief::brief(st.store, me, cursor, now, inner).map_err(internal)?;
            if wrapped_cost(DAEMON_AUTHOR, &b.text) <= max || inner == 0 {
                break b;
            }
            inner = inner * 9 / 10;
        };
        st.store.advance_cursor(me, b.upto).map_err(internal)?;
        ok(b.text)
    }

    /// Inbox cursor lives beside the brief cursor under a reserved name
    /// (`:` can't occur in a valid agent name).
    fn inbox(st: &mut RoomState<'_>, me: &AgentRef, ack: bool) -> Res<Items> {
        let key = AgentRef::new(me.node, format!("inbox:{}", me.agent_name));
        let cursor = st
            .store
            .cursor(&key)
            .map_err(internal)?
            .unwrap_or(Lamport(0));
        let events = st.store.events_since(cursor).map_err(internal)?;
        let upto = events.last().map(|e| e.lamport);
        let my_asks: Vec<EventId> = st
            .store
            .events_since(Lamport(0))
            .map_err(internal)?
            .into_iter()
            .filter(|e| e.author == *me && matches!(e.body, EventBody::Ask { .. }))
            .map(|e| e.id)
            .collect();
        let views = st.store.views();
        let mut items: Items = Vec::new();
        for e in events
            .iter()
            .filter(|e| e.author != *me && views.role_of(&e.author.node).may(e.kind()))
        {
            let text = match &e.body {
                EventBody::Ask { to: Some(t), q } if t == me => {
                    format!("ask id={} | {}", e.id, clean(q, TEXT_CHARS))
                }
                EventBody::Handoff {
                    to: Some(t),
                    summary,
                    refs,
                    next_steps,
                } if t == me => {
                    let refs: Vec<String> = refs.iter().map(ToString::to_string).collect();
                    format!(
                        "handoff id={} | {} | refs: {} | next: {}",
                        e.id,
                        clean(summary, TEXT_CHARS),
                        refs.join(" "),
                        clean(&next_steps.join("; "), TEXT_CHARS)
                    )
                }
                EventBody::Answer { ask_id, a } if my_asks.contains(ask_id) => {
                    format!("answer id={} to={ask_id} | {}", e.id, clean(a, TEXT_CHARS))
                }
                _ => continue,
            };
            if items.len() < INBOX_MAX {
                items.push((agent_label(&e.author), text));
            }
        }
        if ack && let Some(u) = upto {
            st.store.advance_cursor(&key, u).map_err(internal)?;
        }
        Ok(items)
    }

    fn search(st: &RoomState<'_>, query: &str, limit: usize) -> Res<Items> {
        let q = query.to_lowercase();
        let events = st.store.events_since(Lamport(0)).map_err(internal)?;
        let floor = events
            .last()
            .map_or(0, |e| e.lamport.0.saturating_sub(SEARCH_WINDOW));
        let views = st.store.views();
        Ok(events
            .iter()
            .rev()
            .take_while(|e| e.lamport.0 > floor)
            .filter(|e| views.role_of(&e.author.node).may(e.kind()))
            .filter_map(|e| {
                let t = body_text(e)?;
                t.to_lowercase().contains(&q).then(|| {
                    (
                        agent_label(&e.author),
                        format!("id={} {:?}: {}", e.id, e.kind(), clean(&t, 200)),
                    )
                })
            })
            .take(limit.clamp(1, SEARCH_MAX))
            .collect())
    }

    /// Serve one request. Public so tests and embedders can skip IPC.
    pub fn handle_request(&self, req: Request) -> Response {
        if !valid_agent(&req.agent) {
            return Response::Err(ErrCode::BadRequest);
        }
        // Resolve from a snapshot before entering the request's locked section.
        let resolved = match &req.call {
            Call::Ask { to, .. } | Call::Handoff { to: Some(to), .. } => {
                Some(self.resolve(req.room, &req.agent, to))
            }
            _ => None,
        };
        let rooms = lock(&self.rooms);
        let Some(id) = req.room.or(rooms.default) else {
            return Response::Err(ErrCode::NoRoom);
        };
        let Some(entry) = rooms.by_id.get(&id) else {
            return Response::Err(ErrCode::NoRoom);
        };
        let mut guard = lock(&entry.store);
        if let Call::Feed { since, limit } = req.call {
            // Read-only: no `ensure_member`, nothing is signed or published.
            let me = AgentRef::new(self.node, req.agent);
            return match crate::feed::build(
                &entry.name,
                &guard,
                &me,
                (self.clock)(),
                since,
                limit as usize,
            )
            .ok()
            .and_then(|f| serde_json::to_string(&f).ok())
            {
                Some(json) => Response::Ok(vec![("feed".to_owned(), json)]),
                None => Response::Err(ErrCode::Internal),
            };
        }
        if let Call::FeedEvent { id } = &req.call {
            return match crate::feed::event_json(&guard, id) {
                Ok(Some(json)) => Response::Ok(vec![("event".to_owned(), json)]),
                Ok(None) => Response::Err(ErrCode::NotFound),
                Err(_) => Response::Err(ErrCode::Internal),
            };
        }
        let mut st = RoomState {
            name: &entry.name,
            store: &mut guard,
        };
        match self.call(&mut st, &req.agent, req.call, resolved) {
            Ok(items) => Response::Ok(items),
            Err(e) => Response::Err(e),
        }
    }
}

impl Handler for Daemon {
    fn handle(&self, req: Request) -> Response {
        self.handle_request(req)
    }
}

/// Trim a room card so it still fits `ROOM_CARD_MAX` once wrapped.
fn fit_card(card: String) -> String {
    let mut lines: Vec<String> = card.lines().map(str::to_owned).collect();
    // Redundant behind the MCP wrapper first, then least important lines.
    let order: [(&str, bool); 6] = [
        ("(+", false),
        ("«…»", false),
        ("members", false),
        ("lease", true),
        ("task", true),
        ("decision", false),
    ];
    for (prefix, last) in order {
        while wrapped_cost(DAEMON_AUTHOR, &lines.join("\n")) > ROOM_CARD_MAX {
            let pos = if last {
                lines.iter().rposition(|l| l.starts_with(prefix))
            } else {
                lines.iter().position(|l| l.starts_with(prefix))
            };
            match pos {
                Some(p) => {
                    lines.remove(p);
                }
                None => break,
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod resolve_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn ag(node: u8, name: &str) -> AgentRef {
        AgentRef::new(hm_proto::NodeId::from_bytes([node; 32]), name)
    }
    fn room() -> Vec<AgentRef> {
        vec![
            ag(0xa1, "alice-claude"),
            ag(0xa1, "alice-codex"),
            ag(0xb2, "bob-claude"),
            ag(0xc3, "cli"),
        ]
    }

    #[test]
    fn exact_name_and_unique_prefix() {
        assert_eq!(
            pick_target(&room(), "bob-claude"),
            Pick::One(ag(0xb2, "bob-claude"))
        );
        assert_eq!(
            pick_target(&room(), "bob"),
            Pick::One(ag(0xb2, "bob-claude"))
        );
        assert_eq!(pick_target(&room(), "cl"), Pick::One(ag(0xc3, "cli")));
    }

    #[test]
    fn person_slash_agent() {
        assert_eq!(
            pick_target(&room(), "alice/claude"),
            Pick::One(ag(0xa1, "alice-claude"))
        );
        assert_eq!(
            pick_target(&room(), "alice/cod"),
            Pick::One(ag(0xa1, "alice-codex"))
        );
        assert_eq!(pick_target(&room(), "bob/codex"), Pick::None);
        assert_eq!(
            pick_target(&[ag(1, "alice-smith-claude")], "alice-smith/cl"),
            Pick::One(ag(1, "alice-smith-claude"))
        );
    }

    #[test]
    fn node_prefix_slash_name_still_works() {
        assert_eq!(
            pick_target(&room(), "b2b2/bob-claude"),
            Pick::One(ag(0xb2, "bob-claude"))
        );
        assert_eq!(pick_target(&room(), "b2b2/alice-claude"), Pick::None);
    }

    #[test]
    fn ambiguity_and_no_match_never_fall_back() {
        assert!(
            matches!(pick_target(&room(), "alice"), Pick::Many { hits, .. } if hits.len() == 2)
        );
        assert_eq!(pick_target(&room(), "claude"), Pick::None);
        // The old behaviour addressed an unknown local name; now nothing matches.
        assert_eq!(pick_target(&room(), "ghost"), Pick::None);
        assert_eq!(pick_target(&[], "anyone"), Pick::None);
    }

    #[test]
    fn same_name_on_two_machines_is_ambiguous_and_listed_with_nodes() {
        let r = vec![ag(0xa1, "claude"), ag(0xb2, "claude")];
        let Pick::Many { hits, .. } = pick_target(&r, "claude") else {
            panic!("expected Many");
        };
        assert_eq!(names_list(&hits), "claude@a1a1a1, claude@b2b2b2");
        assert_eq!(
            pick_target(&r, "b2b2/claude"),
            Pick::One(ag(0xb2, "claude"))
        );
    }

    #[test]
    fn listing_is_capped_and_handles_empty() {
        assert_eq!(names_list(&[]), "(none yet)");
        let many: Vec<AgentRef> = (0..25).map(|i| ag(1, &format!("a{i:02}"))).collect();
        assert!(names_list(&many).ends_with("and 5 more"));
    }

    #[test]
    fn failed_node_qualification_never_selects_a_person_alias() {
        let agents = vec![ag(1, "deadbe-alice"), ag(2, "b2b2-bob-claude")];
        assert_eq!(pick_target(&agents, "deadbe/alice"), Pick::None);
        assert_eq!(pick_target(&agents, "b2b2/bob-claude"), Pick::None);
    }

    #[test]
    fn qualified_misses_do_not_emit_asks_or_handoffs() {
        let daemon = Daemon::new(SigningKey::from_bytes(&[7; 32]), Arc::new(NullNet));
        let room = daemon
            .create_room(
                "resolve",
                Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
            )
            .unwrap();
        daemon.handle_request(Request {
            room: Some(room),
            agent: "deadbe-alice".into(),
            call: Call::RoomCard,
        });
        for call in [
            Call::Ask {
                to: "deadbe/alice".into(),
                question: "hello".into(),
            },
            Call::Handoff {
                to: Some("deadbe/alice".into()),
                summary: "hello".into(),
                refs: vec![],
                next_steps: vec![],
            },
        ] {
            let response = daemon.handle_request(Request {
                room: Some(room),
                agent: "sender".into(),
                call,
            });
            assert!(
                matches!(response, Response::Ok(items) if items[0].1.contains("Nothing was sent"))
            );
        }
        let store = daemon.store_for_test(room);
        let snapshot = daemon.recipient_snapshot(Some(room), "sender").unwrap();
        assert!(daemon.rooms.try_lock().is_ok());
        assert!(store.try_lock().is_ok());
        assert_eq!(TargetIndex::new(&snapshot).pick("deadbe/alice"), Pick::None);
        assert!(
            !lock(&store)
                .events_since(Lamport(0))
                .unwrap()
                .iter()
                .any(|e| matches!(e.body, EventBody::Ask { .. } | EventBody::Handoff { .. }))
        );
    }

    #[test]
    fn ten_thousand_members_have_bounded_candidates_and_fast_errors() {
        let agents: Vec<_> = (0..10_000)
            .map(|i| ag(1, &format!("alice-worker-{i:05}")))
            .collect();
        let start = std::time::Instant::now();
        assert_eq!(pick_target(&agents, "missing"), Pick::None);
        let listing = names_list(&agents);
        assert_eq!(listing.split(", ").count(), 21);
        assert!(listing.ends_with("and 9980 more"));
        let index = TargetIndex::new(&agents);
        let Pick::Many { hits, total } = index.pick("alice/worker") else {
            panic!("expected ambiguity");
        };
        assert_eq!(total, 10_000);
        assert!(hits.len() <= 20, "candidate collection must be capped");
        assert!(
            index
                .names_list(hits.iter(), total)
                .ends_with("and 9980 more")
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn oversized_peer_names_are_not_indexed_or_resolvable() {
        let evil = format!("zed{}", "-".repeat(60_000));
        let agents = vec![ag(1, "alice"), ag(2, &evil)];
        let start = std::time::Instant::now();
        let index = TargetIndex::new(&agents);
        assert!(start.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(index.indexed, 1);
        assert!(index.people.len() <= 32);
        assert_eq!(index.pick("z"), Pick::None);
        assert_eq!(index.pick(&evil), Pick::None);
        assert_eq!(index.pick("zed/-"), Pick::None);
        assert!(matches!(index.pick("alice"), Pick::One(_)));
        assert_eq!(names_list(&agents), "alice");
    }

    #[test]
    fn ten_thousand_duplicate_names_have_fast_bounded_errors() {
        let agents: Vec<_> = (0u32..10_000)
            .map(|i| {
                let mut node = [0; 32];
                node[..4].copy_from_slice(&i.to_be_bytes());
                AgentRef::new(NodeId::from_bytes(node), "worker")
            })
            .collect();
        let start = std::time::Instant::now();
        assert!(names_list(&agents).split(", ").count() <= 21);
        let Pick::Many { hits, .. } = pick_target(&agents, "worker") else {
            panic!("expected ambiguity");
        };
        assert!(hits.len() <= 20);
        // Exact node lookup must still find a duplicate beyond the retained candidates.
        assert_eq!(
            pick_target(&agents, &format!("{}/worker", agents[9999].node)),
            Pick::One(agents[9999].clone())
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
