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
const AGENT_NAME_MAX: usize = 24;
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

fn valid_agent(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= AGENT_NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        // Names are signed into every event and shown to peers (N7).
        && !hm_proto::redact::contains_secret(name)
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

    /// `node6/name`, a bare name known in the room, or a bare local name.
    fn resolve(&self, st: &RoomState<'_>, to: &str) -> Res<AgentRef> {
        let members = &st.store.views().members;
        if let Some((node, name)) = to.split_once('/') {
            if node.len() < 4 || !valid_agent(name) {
                return Err(ErrCode::BadRequest);
            }
            let mut hits = members
                .keys()
                .filter(|a| a.agent_name == name && a.node.to_string().starts_with(node));
            return match (hits.next(), hits.next()) {
                (Some(a), None) => Ok(a.clone()),
                _ => Err(ErrCode::NotFound),
            };
        }
        if !valid_agent(to) {
            return Err(ErrCode::BadRequest);
        }
        let mut hits = members.keys().filter(|a| a.agent_name == to);
        match (hits.next(), hits.next()) {
            (Some(a), None) => Ok(a.clone()),
            (None, _) => Ok(AgentRef::new(self.node, to)),
            _ => Err(ErrCode::BadRequest),
        }
    }

    fn call(&self, st: &mut RoomState<'_>, agent: &str, call: Call) -> Res<Items> {
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
            Call::Ask { to, question } => {
                let to = self.resolve(st, &to)?;
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
                let to = to.map(|t| self.resolve(st, &t)).transpose()?;
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
        let rooms = lock(&self.rooms);
        let Some(id) = req.room.or(rooms.default) else {
            return Response::Err(ErrCode::NoRoom);
        };
        let Some(entry) = rooms.by_id.get(&id) else {
            return Response::Err(ErrCode::NoRoom);
        };
        let mut guard = lock(&entry.store);
        let mut st = RoomState {
            name: &entry.name,
            store: &mut guard,
        };
        match self.call(&mut st, &req.agent, req.call) {
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
