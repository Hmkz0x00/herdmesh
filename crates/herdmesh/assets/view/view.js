/* herdmesh live room view (ADR-003).
 * Part 1: pure functions (state, event planning, animation queue, demo script), unit-tested with node --test.
 * Part 2: DOM layer. Every API string is untrusted: it only ever reaches the page through textContent
 * (or setAttribute of plain text attributes such as title). No markup parsing of API data anywhere. */
(function (root) {
  'use strict';

  /* ---------------- Part 1: pure core ---------------- */
  var FEED_MAX = 60, SEEN_MAX = 1000, TEXT_MAX = 200, BUBBLE_MAX = 90;
  var STATUSES = ['idle', 'working', 'blocked', 'done', 'unknown'];
  var ROLES = ['owner', 'member', 'observer'];
  var LABELS = { join: 'joined', leave: 'left', ask: 'ask', answer: 'answer', handoff: 'handoff', claim: 'claim',
    release: 'release', task_add: 'new task', task_update: 'task', decide: 'decision', note: 'note', lost: 'claim lost', presence: 'presence' };
  var TRAVEL = { ask: 1, answer: 1, handoff: 1 };

  function clip(s, n) {
    s = String(s == null ? '' : s);
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  }
  function shortName(id) {
    id = String(id == null ? '' : id);
    var i = id.indexOf('/');
    return i >= 0 ? id.slice(i + 1) : id;
  }
  function nodeOf(id) {
    id = String(id == null ? '' : id);
    var i = id.indexOf('/');
    return i >= 0 ? id.slice(0, i) : id;
  }
  function hueFor(str) {
    var h = 7, s = String(str == null ? '' : str);
    for (var i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
    return h % 360;
  }
  function normStatus(s) { return STATUSES.indexOf(s) >= 0 ? s : 'unknown'; }
  function normRole(r) { return ROLES.indexOf(r) >= 0 ? r : 'member'; }
  function pad2(n) { return n < 10 ? '0' + n : String(n); }
  function fmtCountdown(ms) {
    if (!(ms > 0)) return '0:00';
    var s = Math.ceil(ms / 1000);
    return Math.floor(s / 60) + ':' + pad2(s % 60);
  }
  function fmtAge(ms) {
    if (!(ms >= 5000)) return 'now';
    var s = Math.floor(ms / 1000);
    if (s < 60) return s + 's';
    if (s < 3600) return Math.floor(s / 60) + 'm';
    return Math.floor(s / 3600) + 'h';
  }
  function fmtClock(ts) {
    if (!(ts > 1e11)) return '';
    var d = new Date(ts);
    return pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  }
  /* 0..1 fraction of a lease still remaining */
  function leaseFraction(remainingMs, totalMs) {
    if (!(totalMs > 0) || !(remainingMs > 0)) return 0;
    return Math.min(1, remainingMs / totalMs);
  }

  function emptyState() {
    return { room: '', you: '', members: new Map(), leases: [], tasks: [], cursor: 0, offset: 0,
      seen: new Set(), feed: [], leaseTotal: {} };
  }

  /* leases that vanished without a release by their holder in the same batch are "lost" */
  function diffLeases(prev, next, events, now) {
    var present = new Set();
    (next || []).forEach(function (l) { present.add(l && l.id); });
    var out = [];
    (prev || []).forEach(function (l) {
      if (!l || present.has(l.id)) return;
      var released = (events || []).some(function (e) {
        return e && e.kind === 'release' && e.from === l.holder && (e.ref == null || e.ref === l.id);
      });
      if (released) return;
      var reason = l.state === 'pending' ? 'denied'
        : (l.expires_ms && l.expires_ms <= now + 1500 ? 'expired' : 'revoked');
      out.push({ lease: l, reason: reason });
    });
    return out;
  }

  function cleanEvent(e) {
    return { id: String(e.id), lamport: e.lamport | 0, ts_ms: +e.ts_ms || 0, kind: String(e.kind || 'note'),
      from: e.from == null ? null : String(e.from), to: e.to == null ? null : String(e.to),
      text: clip(e.text, TEXT_MAX), ref: e.ref == null ? null : String(e.ref), full: e.full === true };
  }

  /* Merge a /api/state snapshot. Returns { state, fresh, lost }; `fresh` are unseen events in causal order
   * (plus synthetic "lost" entries), never mutating `prev`. */
  function applySnapshot(prev, snap, localNow) {
    if (!snap || typeof snap !== 'object') return { state: prev, fresh: [], lost: [] };
    var seen = new Set(prev.seen);
    var serverNow = +snap.now_ms || 0;
    var offset = serverNow ? serverNow - localNow : prev.offset;
    var effNow = localNow + offset;

    var members = new Map();
    (Array.isArray(snap.members) ? snap.members : []).forEach(function (m) {
      if (!m || m.id == null) return;
      var id = String(m.id);
      members.set(id, { id: id, node: String(m.node == null ? nodeOf(id) : m.node), name: clip(m.name || shortName(id), 40),
        role: normRole(m.role), local: m.local === true, present: m.present !== false, status: normStatus(m.status),
        activity: clip(m.activity, TEXT_MAX), last_ms: +m.last_ms || 0 });
    });

    var rawEvents = (Array.isArray(snap.events) ? snap.events : []).filter(function (e) {
      return e && e.id != null && !seen.has(String(e.id));
    });
    rawEvents.sort(function (a, b) { return ((a.lamport | 0) - (b.lamport | 0)) || ((+a.ts_ms || 0) - (+b.ts_ms || 0)); });
    var fresh = [];
    rawEvents.forEach(function (e) {
      var c = cleanEvent(e);
      if (seen.has(c.id)) return;
      seen.add(c.id);
      fresh.push(c);
    });
    while (seen.size > SEEN_MAX) seen.delete(seen.values().next().value);

    var leases = (Array.isArray(snap.leases) ? snap.leases : []).filter(function (l) { return l && l.id != null; }).map(function (l) {
      return { id: String(l.id), paths: (Array.isArray(l.paths) ? l.paths : []).map(function (p) { return clip(p, 120); }),
        holder: String(l.holder == null ? '' : l.holder), state: l.state === 'granted' ? 'granted' : 'pending', expires_ms: +l.expires_ms || 0 };
    });
    var leaseTotal = {};
    leases.forEach(function (l) {
      var old = prev.leaseTotal[l.id];
      leaseTotal[l.id] = old || (l.expires_ms ? Math.max(1000, l.expires_ms - effNow) : 0);
    });

    var lost = diffLeases(prev.leases, leases, fresh, effNow);
    lost.forEach(function (x) {
      var who = (members.get(x.lease.holder) || prev.members.get(x.lease.holder) || {}).name || shortName(x.lease.holder);
      fresh.push({ id: 'lost-' + x.lease.id, lamport: (snap.cursor | 0) || prev.cursor, ts_ms: serverNow || localNow, kind: 'lost',
        from: x.lease.holder, to: null, text: who + ' lost ' + x.lease.paths.join(', ') + ' (' + x.reason + ')', ref: x.lease.id, full: true });
    });

    var tasks = (Array.isArray(snap.tasks) ? snap.tasks : []).filter(function (t) { return t && t.id != null; }).map(function (t) {
      return { id: String(t.id), title: clip(t.title, TEXT_MAX), owner: t.owner == null ? null : String(t.owner),
        status: ['open', 'doing', 'blocked', 'done'].indexOf(t.status) >= 0 ? t.status : 'open' };
    });

    var feed = fresh.slice().reverse().concat(prev.feed).slice(0, FEED_MAX);
    return { state: { room: String(snap.room == null ? prev.room : snap.room), you: String(snap.you == null ? prev.you : snap.you),
      members: members, leases: leases, tasks: tasks, cursor: typeof snap.cursor === 'number' ? snap.cursor : prev.cursor,
      offset: offset, seen: seen, feed: feed, leaseTotal: leaseTotal }, fresh: fresh, lost: lost };
  }

  /* How should an event be animated? motion: travel | pop | none */
  function planEvent(ev, members) {
    var label = LABELS[ev.kind] || 'note';
    var motion = 'pop';
    if (ev.kind === 'presence') motion = 'none';
    else if (TRAVEL[ev.kind] && ev.to && ev.to !== ev.from && members.has(ev.from) && members.has(ev.to)) motion = 'travel';
    else if (!ev.from || !members.has(ev.from)) motion = 'none';
    var fromM = members.get(ev.from), toM = ev.to ? members.get(ev.to) : null;
    var text = ev.text;
    if (!text && ev.kind === 'join') text = 'joined the room';
    if (!text && ev.kind === 'leave') text = 'left the room';
    return { id: ev.id, full: ev.full === true, kind: ev.kind, motion: motion, from: ev.from, to: ev.to, label: label, text: clip(text, BUBBLE_MAX),
      fromName: fromM ? fromM.name : shortName(ev.from), toName: toM ? toM.name : (ev.to ? shortName(ev.to) : '') };
  }

  /* ids arrive as full hex; show a short prefix */
  function shortId(id) { return String(id == null ? '' : id).slice(0, 6); }
  /* One conversation row for the feed: who -> whom, kind, text, time. */
  function feedRow(ev, members) {
    var f = members.get(ev.from), t = ev.to ? members.get(ev.to) : null;
    return { id: ev.id, shortId: shortId(ev.id), kind: ev.kind, label: LABELS[ev.kind] || ev.kind,
      fromName: f ? f.name : shortName(ev.from), fromLocal: f ? f.local : null,
      toName: ev.to ? (t ? t.name : shortName(ev.to)) : '', toLocal: t ? t.local : null,
      text: ev.text, truncated: ev.full !== true, time: fmtClock(ev.ts_ms) };
  }
  function eventUrl(id) { return 'api/event?id=' + encodeURIComponent(String(id)); }
  /* what we can show instantly, before (or without) the api/event response */
  function detailFromEvent(ev) {
    return { id: ev.id, kind: ev.kind, from: ev.from, to: ev.to, ts_ms: ev.ts_ms, text: ev.text, refs: ev.ref ? [ev.ref] : [], complete: ev.full === true };
  }
  /* validate an api/event response; returns null if unusable */
  function parseDetail(o) {
    if (!o || typeof o !== 'object' || o.id == null) return null;
    return { id: String(o.id), kind: String(o.kind || 'note'), from: o.from == null ? null : String(o.from), to: o.to == null ? null : String(o.to),
      ts_ms: +o.ts_ms || 0, text: String(o.text == null ? '' : o.text).slice(0, 20000),
      refs: (Array.isArray(o.refs) ? o.refs : []).slice(0, 50).map(function (r) { return clip(r, 80); }), complete: true };
  }

  /* Animation queue: pure, caller supplies the clock. Bigger backlog => shorter gap so we never fall far behind. */
  function createQueue() { return { items: [], lastAt: -Infinity }; }
  function queueGap(n) { return n > 12 ? 90 : n > 6 ? 220 : 520; }
  function queuePush(q, plans) {
    plans.forEach(function (p) { if (p.motion !== 'none') q.items.push(p); });
    var MAX = 30;
    while (q.items.length > MAX) {
      var i = q.items.findIndex(function (p) { return p.motion === 'pop'; });
      q.items.splice(i < 0 ? 0 : i, 1);
    }
  }
  function queueTake(q, now, inflight, maxInflight) {
    if (!q.items.length || inflight >= (maxInflight || 5)) return [];
    if (now - q.lastAt < queueGap(q.items.length)) return [];
    q.lastAt = now;
    return [q.items.shift()];
  }

  /* Scripted demo room. Returns frames [{at, snap}] (at = ms from start) plus total length. */
  function buildDemo(base, extra) {
    var A = '3d053a/alice', B = '1e048f/bob';
    var m = { members: [], leases: [], tasks: [], lam: 0 };
    var frames = [], pend = [], details = {};
    function clone(x) { return JSON.parse(JSON.stringify(x)); }
    function ev(at, kind, from, to, text, ref, fullText) {
      m.lam++;
      var id = ((m.lam * 2654435761) >>> 0).toString(16) + ((m.lam * 40503) >>> 0).toString(16) + 'c0ffee00';
      id = id.slice(0, 16);
      pend.push({ id: id, lamport: m.lam, ts_ms: base + at, kind: kind, from: from, to: to || null, text: text || '', ref: ref || null, full: !fullText });
      details[id] = { id: id, kind: kind, from: from, to: to || null, ts_ms: base + at, text: fullText || text || '', refs: ref ? [ref] : [] };
    }
    function mem(id, patch) {
      var x = m.members.filter(function (q) { return q.id === id; })[0];
      if (!x) { x = { id: id, node: nodeOf(id), name: shortName(id), role: 'member', local: false, present: true, status: 'idle', activity: '', last_ms: 0 }; m.members.push(x); }
      for (var k in patch) x[k] = patch[k];
    }
    function frame(at, fn) {
      fn();
      frames.push({ at: at, snap: { room: 'demo-room', you: A, now_ms: base + at, cursor: m.lam, members: clone(m.members),
        leases: clone(m.leases), tasks: clone(m.tasks), events: pend } });
      pend = [];
    }
    function lease(id) { return m.leases.filter(function (l) { return l.id === id; })[0]; }
    var S = 1000;
    var extras = Math.max(0, Math.min(6, extra | 0));
    var names = ['dana/gemini', 'eli/claude', 'fay/codex', 'gus/claude', 'hal/codex', 'ivy/gemini'];
    var sts = ['working', 'idle', 'blocked', 'working', 'done', 'idle'];
    frame(0, function () {
      mem(A, { name: 'alice/claude', role: 'owner', local: true, status: 'idle', activity: 'just joined' });
      ev(0, 'join', A, null, 'alice/claude joined');
      for (var i = 0; i < extras; i++) {
        var id = (200 + i) + 'a/' + names[i].split('/')[0];
        mem(id, { name: names[i], local: i % 2 === 0, status: sts[i], activity: 'holds docs/** · reviewing a long pull request description here' });
      }
    });
    frame(1.5 * S, function () { mem(B, { name: 'bob/codex', status: 'idle', activity: 'just joined' }); ev(1.5 * S, 'join', B, null, 'bob/codex joined'); });
    frame(3 * S, function () { mem(A, { status: 'working', activity: 'reading src/net/sync.rs' }); });
    frame(4.5 * S, function () { ev(4.5 * S, 'ask', A, B, 'Which port does the relay use for sync? Also: is TCP fallback on?', null, 'Which port does the relay use for sync? Also: is TCP fallback on, and does the 30s keepalive apply to relayed streams too? I need both before I touch src/net/sync.rs, because the reconnect backoff depends on it.'); mem(A, { status: 'blocked', activity: 'waiting on bob · asked 0s ago' }); mem(B, { status: 'working', activity: 'reading the question from alice' }); });
    frame(7.5 * S, function () { ev(7.5 * S, 'answer', B, A, '443 via the relay; direct UDP is tried first. TCP fallback: yes.', null, '443 via the relay; direct UDP is tried first. TCP fallback is on. The 30s keepalive applies to relayed streams as well (see relay.rs ping loop), so base the reconnect backoff on 30s, not 10s.'); mem(B, { status: 'idle', activity: 'answered alice just now' }); mem(A, { status: 'working', activity: 'editing src/net/sync.rs' }); });
    frame(10 * S, function () {
      m.leases.push({ id: 'ab12', paths: ['src/net/**'], holder: A, state: 'granted', expires_ms: base + 22 * S });
      ev(10 * S, 'claim', A, null, 'src/net/**', 'ab12'); mem(A, { activity: 'holds src/net/** · editing' });
    });
    frame(11.5 * S, function () {
      m.tasks.push({ id: 't-3', title: 'fix sync reconnect', status: 'doing', owner: A });
      ev(11.5 * S, 'task_add', A, null, 'fix sync reconnect', 't-3'); mem(A, { activity: 'holds src/net/** · t-3 doing' });
    });
    frame(13.5 * S, function () {
      m.leases.push({ id: 'cd34', paths: ['src/net/relay.rs'], holder: B, state: 'pending', expires_ms: base + 40 * S });
      ev(13.5 * S, 'claim', B, null, 'src/net/relay.rs (waiting)', 'cd34'); mem(B, { status: 'blocked', activity: 'waiting for alice’s claim on src/net/**' });
    });
    frame(17 * S, function () {
      m.tasks.push({ id: 't-4', title: 'relay retry on 429', status: 'open', owner: null });
      ev(17 * S, 'task_add', B, null, 'relay retry on 429', 't-4');
    });
    frame(23 * S, function () {
      m.leases = m.leases.filter(function (l) { return l.id !== 'ab12'; });
      var l = lease('cd34'); l.state = 'granted'; l.expires_ms = base + 52 * S;
      mem(A, { status: 'idle', activity: 'lost claim src/net/** (expired)' }); mem(B, { status: 'working', activity: 'holds src/net/relay.rs · editing' });
    });
    frame(25.5 * S, function () {
      m.tasks[0].status = 'blocked'; ev(25.5 * S, 'task_update', A, null, 't-3 blocked: claim expired', 't-3');
      mem(A, { status: 'blocked', activity: 't-3 blocked · claim expired' });
    });
    frame(28 * S, function () {
      m.tasks[0].status = 'done'; m.tasks[1].status = 'doing'; m.tasks[1].owner = B;
      ev(28 * S, 'task_update', A, null, 't-3 done', 't-3'); mem(A, { status: 'working', activity: 'finishing t-3 patch' });
    });
    frame(30 * S, function () {
      ev(30 * S, 'handoff', A, B, 'Review the t-3 patch: relay retry on 429', 't-3', 'Review the t-3 patch: relay retry on 429. Changes are in src/net/sync.rs (backoff 30s base, jitter 20%) and the new test sync_reconnect. I did not touch relay.rs. Open question: should 429 honour Retry-After? I assumed yes.'); mem(A, { status: 'idle', activity: 'handed t-3 to bob' });
      mem(B, { status: 'working', activity: 'reviewing t-3 · t-4 doing' });
    });
    frame(33 * S, function () { ev(33 * S, 'decide', B, null, 'Relay first, direct UDP as an upgrade.', null, 'Decision: connect via the relay first, then try to upgrade to direct UDP in the background. Reason: relay always works behind NAT, so first-byte latency wins over path optimality. Revisit if relay cost grows.'); });
    frame(36 * S, function () {
      m.leases = []; ev(36 * S, 'release', B, null, 'src/net/relay.rs', 'cd34'); m.tasks[1].status = 'done';
      mem(B, { status: 'idle', activity: 'released src/net/relay.rs' });
    });
    return { frames: frames, length: 40 * S, details: details };
  }

  var core = { clip: clip, shortName: shortName, nodeOf: nodeOf, hueFor: hueFor, normStatus: normStatus, fmtCountdown: fmtCountdown,
    fmtAge: fmtAge, fmtClock: fmtClock, leaseFraction: leaseFraction, emptyState: emptyState, diffLeases: diffLeases,
    applySnapshot: applySnapshot, planEvent: planEvent, createQueue: createQueue, queueGap: queueGap, queuePush: queuePush,
    queueTake: queueTake, buildDemo: buildDemo, shortId: shortId, feedRow: feedRow, eventUrl: eventUrl,
    detailFromEvent: detailFromEvent, parseDetail: parseDetail, LABELS: LABELS };

  if (typeof module === 'object' && module && module.exports) { module.exports = core; return; }
  if (typeof document === 'undefined') return;

  /* ---------------- Part 2: DOM layer ---------------- */
  var doc = document;
  function $(id) { return doc.getElementById(id); }
  function h(tag, cls, text) {
    var e = doc.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }
  var STATUS_GLYPH = { working: '●', blocked: '■', idle: '○', done: '✓', unknown: '?' };
  var STATUS_TEXT = { working: 'working', blocked: 'blocked', idle: 'idle', done: 'done', unknown: 'unknown' };
  var params = new URLSearchParams(root.location.search);
  var demo = root.location.protocol === 'file:' || params.get('demo') === '1';
  var state = core.emptyState();
  var queue = core.createQueue();
  var agentEls = new Map(), claimEls = new Map(), ghosts = new Map();
  var inflight = 0, gotFirst = false;
  var els = { stage: $('stage'), fx: $('fx'), local: $('col-local'), remote: $('col-remote'), claims: $('claims'),
    tasks: $('tasks'), taskN: $('task-n'), feed: $('feed'), room: $('room'), conn: $('conn'), mode: $('mode'), sr: $('sr') };
  var motionQuery = root.matchMedia ? root.matchMedia('(prefers-reduced-motion: reduce)') : null;
  function reduced() { return !!(motionQuery && motionQuery.matches); }
  function serverNow() { return Date.now() + state.offset; }
  function memberName(id) { var m = state.members.get(id); return m ? m.name : core.shortName(id); }

  /* theme */
  var KEY = 'herdmesh-view-theme';
  function applyTheme(t) { if (t) doc.documentElement.setAttribute('data-theme', t); else doc.documentElement.removeAttribute('data-theme'); }
  try { applyTheme(root.localStorage.getItem(KEY)); } catch (e) { /* storage unavailable: follow the system theme */ }
  $('theme').addEventListener('click', function () {
    var cur = doc.documentElement.getAttribute('data-theme');
    var dark = cur ? cur === 'dark' : !!(root.matchMedia && root.matchMedia('(prefers-color-scheme: dark)').matches);
    var next = dark ? 'light' : 'dark';
    applyTheme(next);
    try { root.localStorage.setItem(KEY, next); } catch (e) { /* ignore */ }
  });

  els.mode.textContent = demo ? 'DEMO' : 'LIVE';
  function setConn(ok) {
    els.conn.className = 'pill ' + (ok ? 'live' : 'bad');
    els.conn.textContent = demo ? 'demo loop' : ok ? 'connected' : 'reconnecting…';
  }

  /* agents */
  function buildAgent() {
    var a = h('div', 'agent'), bot = h('div', 'bot'), eyes = h('div', 'eyes');
    eyes.append(h('span', 'eye'), h('span', 'eye')); bot.append(eyes);
    var meta = h('div', 'meta'), nm = h('div', 'nm'), sub = h('div', 'sub');
    var side = h('span', 'badge side'), role = h('span', 'badge role'), node = h('span', 'node');
    sub.append(side, role, node); meta.append(nm, sub);
    var stat = h('div', 'stat'), dot = h('span', 'dot'), st = h('span', 'stxt');
    stat.append(dot, st);
    var act = h('div', 'act');
    a.append(bot, meta, stat, act);
    return { root: a, bot: bot, nm: nm, side: side, role: role, node: node, dot: dot, st: st, act: act };
  }
  function updateAgent(rec, m) {
    var r = rec.root;
    r.className = 'agent ' + (m.local ? 'local' : 'remote') + ' st-' + m.status + (m.present ? '' : ' away');
    r.style.setProperty('--hue', String(core.hueFor(m.id)));
    rec.nm.textContent = m.name;
    rec.side.textContent = m.local ? 'local' : 'remote';
    rec.role.textContent = m.role;
    rec.node.textContent = m.node;
    rec.dot.textContent = STATUS_GLYPH[m.status];
    rec.st.textContent = STATUS_TEXT[m.status] + (m.present ? '' : ' · away');
    var text = m.activity || (m.present ? 'no activity yet' : 'not in the room');
    rec.act.textContent = text; rec.act.setAttribute('title', text);
    rec.root.setAttribute('role', 'group');
    rec.root.setAttribute('aria-label', m.name + ', ' + (m.local ? 'local' : 'remote') + ', ' + m.status);
  }
  function renderMembers() {
    var keep = new Set();
    state.members.forEach(function (m, id) {
      keep.add(id);
      var rec = agentEls.get(id);
      if (!rec) { rec = buildAgent(); agentEls.set(id, rec); }
      updateAgent(rec, m);
      var col = m.local ? els.local : els.remote;
      if (rec.root.parentNode !== col) col.append(rec.root);
    });
    agentEls.forEach(function (rec, id) {
      if (keep.has(id)) return;
      agentEls.delete(id);
      rec.root.classList.add('leaving');
      setTimeout(function () { rec.root.remove(); syncEmpty(); }, reduced() ? 0 : 400);
    });
    syncEmpty();
  }
  function setEmpty(col, text) {
    var has = Array.prototype.some.call(col.children, function (c) { return c.classList.contains('agent') || c.classList.contains('claim'); });
    var ph = col.querySelector('.empty');
    if (has && ph) ph.remove();
    if (!has && !ph) col.append(h('div', 'empty', text));
  }
  function syncEmpty() {
    setEmpty(els.local, 'Nobody local yet');
    setEmpty(els.remote, 'Waiting for a peer to join');
    setEmpty(els.claims, 'No claims. Agents claim paths before editing.');
  }

  /* claims */
  function renderClaims(lost) {
    var now = serverNow(), keep = new Set();
    lost.forEach(function (x) { ghosts.set(x.lease.id, { lease: x.lease, reason: x.reason, until: Date.now() + 10000 }); });
    state.leases.forEach(function (l) { keep.add(l.id); ghosts.delete(l.id); drawClaim(l, now, null); });
    ghosts.forEach(function (g, id) {
      if (g.until < Date.now()) { ghosts.delete(id); return; }
      keep.add(id); drawClaim(g.lease, now, g.reason);
    });
    claimEls.forEach(function (rec, id) { if (!keep.has(id)) { rec.root.remove(); claimEls.delete(id); } });
    syncEmpty();
  }
  function drawClaim(l, now, lostReason) {
    var rec = claimEls.get(l.id);
    if (!rec) {
      var r = h('div', 'claim'), paths = h('div', 'paths'), who = h('div', 'who'), tag = h('span', 'tag'), by = h('span', 'by'), cd = h('span', 'cd');
      who.append(tag, by, cd);
      var bar = h('div', 'bar'), fill = h('i'); bar.append(fill);
      r.append(paths, who, bar);
      rec = { root: r, paths: paths, tag: tag, by: by, cd: cd, fill: fill, bar: bar };
      claimEls.set(l.id, rec); els.claims.append(r);
    }
    var holder = state.members.get(l.holder);
    var remote = holder ? !holder.local : false;
    rec.root.className = 'claim' + (remote ? ' remote' : '') + (lostReason ? ' lost' : l.state === 'pending' ? ' pending' : '');
    rec.paths.textContent = l.paths.join(', ') || '(no paths)';
    rec.by.textContent = holder ? holder.name : core.shortName(l.holder);
    if (lostReason) {
      rec.tag.textContent = 'lost · ' + lostReason; rec.cd.textContent = ''; rec.bar.hidden = true;
      rec.fill.style.setProperty('--f', '0');
    } else {
      rec.bar.hidden = !l.expires_ms;
      rec.tag.textContent = l.state === 'pending' ? 'waiting' : 'held';
      var remaining = l.expires_ms ? l.expires_ms - now : 0;
      rec.cd.textContent = l.expires_ms ? core.fmtCountdown(remaining) : '';
      rec.fill.style.setProperty('--f', String(core.leaseFraction(remaining, state.leaseTotal[l.id])));
    }
  }

  /* tasks */
  function renderTasks() {
    var order = { blocked: 0, doing: 1, open: 2, done: 3 };
    var open = state.tasks.filter(function (t) { return t.status !== 'done'; }).sort(function (a, b) { return order[a.status] - order[b.status]; });
    var done = state.tasks.length - open.length;
    els.taskN.textContent = open.length + (done ? ' · ' + done + ' done' : '');
    var kids = open.map(function (t) {
      var li = h('li', 'task t-' + t.status);
      li.append(h('span', 'tid', t.id), h('span', 'tt', t.title));
      li.append(h('span', 'tm', t.status + (t.owner ? ' · ' + memberName(t.owner) : ' · unassigned')));
      return li;
    });
    if (!kids.length) kids.push(h('li', 'empty', 'No open tasks'));
    els.tasks.replaceChildren.apply(els.tasks, kids);
  }

  /* feed */
  function sideClass(local) { return local === true ? 'ml' : local === false ? 'mr' : 'mu'; }
  function feedAdd(events) {
    events.forEach(function (e) {
      if (e.kind === 'presence') return;
      var row = core.feedRow(e, state.members);
      var li = h('li', 'fi k-' + e.kind), btn = h('button', 'fb');
      btn.type = 'button';
      btn.setAttribute('aria-label', row.label + ' from ' + row.fromName + (row.toName ? ' to ' + row.toName : '') + ', open details');
      var head = h('span', 'fh'), w = h('span', 'w');
      head.append(h('span', 'k', row.label), w);
      w.append(h('span', 'wn ' + sideClass(row.fromLocal), row.fromName));
      if (row.toName) w.append(h('span', 'ar', ' \u2192 '), h('span', 'wn ' + sideClass(row.toLocal), row.toName));
      head.append(h('time', null, row.time));
      btn.append(head);
      if (row.text) btn.append(h('span', 'x', row.text));
      btn.append(h('span', 'fo', '#' + row.shortId + (row.truncated ? ' \u00b7 shortened, open for full text' : '')));
      btn.addEventListener('click', function () { openDetail(core.detailFromEvent(e), btn); });
      li.append(btn);
      els.feed.prepend(li);
    });
    while (els.feed.children.length > 60) els.feed.lastElementChild.remove();
  }

  /* detail panel: full text of one message, fetched from api/event (scripted in demo mode) */
  var dEl = { root: $('detail'), close: $('d-close'), kind: $('d-kind'), who: $('d-who'), time: $('d-time'), id: $('d-id'),
    text: $('d-text'), refs: $('d-refs'), note: $('d-note') };
  var detailSeq = 0, detailTrigger = null, demoDetails = {};
  function showDetail(d, note) {
    dEl.kind.textContent = core.LABELS[d.kind] || d.kind;
    dEl.root.className = 'detail k-' + d.kind;
    dEl.who.textContent = memberName(d.from) + (d.to ? ' \u2192 ' + memberName(d.to) : '');
    dEl.time.textContent = core.fmtClock(d.ts_ms);
    dEl.id.textContent = '#' + core.shortId(d.id);
    dEl.text.textContent = d.text || '(no text)';
    dEl.refs.textContent = d.refs.length ? 'refs: ' + d.refs.join(', ') : '';
    dEl.note.textContent = note || '';
  }
  function openDetail(d, trigger) {
    var seq = ++detailSeq;
    detailTrigger = trigger || null;
    showDetail(d, d.complete ? '' : 'Loading full text\u2026');
    dEl.root.hidden = false;
    dEl.close.focus();
    function got(full) {
      if (seq !== detailSeq || dEl.root.hidden) return;
      var p = core.parseDetail(full);
      if (p) { if (!p.ts_ms) p.ts_ms = d.ts_ms; showDetail(p, ''); } else fail();
    }
    function fail() {
      if (seq !== detailSeq || dEl.root.hidden) return;
      dEl.note.textContent = d.complete ? '' : 'Full text unavailable; showing the shortened text.';
    }
    if (demo) { setTimeout(function () { var x = demoDetails[d.id]; if (x) got(x); else fail(); }, 150); return; }
    var ctl = new AbortController(), kill = setTimeout(function () { ctl.abort(); }, 4000);
    fetch(core.eventUrl(d.id), { cache: 'no-store', credentials: 'same-origin', signal: ctl.signal })
      .then(function (res) { if (!res.ok) throw new Error('http ' + res.status); return res.json(); })
      .then(function (o) { clearTimeout(kill); got(o); })
      .catch(function () { clearTimeout(kill); fail(); });
  }
  function closeDetail() {
    if (dEl.root.hidden) return;
    detailSeq++; dEl.root.hidden = true;
    if (detailTrigger && detailTrigger.isConnected) detailTrigger.focus();
    detailTrigger = null;
  }
  dEl.close.addEventListener('click', closeDetail);
  doc.addEventListener('keydown', function (e) { if (e.key === 'Escape') closeDetail(); });

  /* bubbles */
  function center(el, base) {
    var r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2 - base.left, y: r.top + r.height / 2 - base.top, w: r.width, h: r.height };
  }
  function bubbleEl(plan, withName) {
    var b = h('div', 'bub k-' + plan.kind);
    b.append(h('b', null, withName ? plan.fromName + ' · ' + plan.label : plan.label));
    b.setAttribute('title', 'Click for details');
    b.addEventListener('click', function () {
      openDetail(core.detailFromEvent({ id: plan.id, kind: plan.kind, from: plan.from, to: plan.to, ts_ms: 0, text: plan.text, ref: null, full: plan.full }), null);
    });
    if (plan.text) b.append(h('span', null, plan.text));
    return b;
  }
  function launch(b, frames, opts) {
    if (els.fx.children.length > 14) els.fx.firstElementChild.remove();
    els.fx.append(b);
    inflight++;
    var done = false;
    function end() { if (done) return; done = true; inflight = Math.max(0, inflight - 1); b.remove(); }
    if (!b.animate) { setTimeout(end, 1500); return; }
    var an = b.animate(frames, opts);
    an.addEventListener('finish', end); an.addEventListener('cancel', end);
  }
  function tr(x, y, s) { return 'translate(' + Math.round(x) + 'px,' + Math.round(y) + 'px) translate(-50%,-50%) scale(' + s + ')'; }
  function playPlan(plan) {
    var fromRec = agentEls.get(plan.from), toRec = plan.to ? agentEls.get(plan.to) : null;
    if (!fromRec) return;
    var base = els.fx.getBoundingClientRect();
    var a = center(fromRec.bot, base);
    if (plan.motion === 'travel' && toRec) {
      var b = center(toRec.bot, base);
      var dx = b.x - a.x, dy = b.y - a.y, len = Math.sqrt(dx * dx + dy * dy) || 1;
      var stop = Math.min(len * 0.25, 70);              // stop short so the receiver stays visible
      var ex = b.x - dx / len * stop, ey = b.y - dy / len * stop;
      if (reduced()) {
        launch(bubbleEl(plan, true), [{ transform: tr(ex, ey, 1), opacity: 0 }, { transform: tr(ex, ey, 1), opacity: 1, offset: .12 },
          { transform: tr(ex, ey, 1), opacity: 1, offset: .85 }, { transform: tr(ex, ey, 1), opacity: 0 }], { duration: 2600 });
        return;
      }
      var tl = h('div', 'trail k-' + plan.kind);
      tl.style.width = Math.round(len) + 'px';
      tl.style.transform = 'translate(' + Math.round(a.x) + 'px,' + Math.round(a.y) + 'px) rotate(' + Math.atan2(dy, dx) + 'rad)';
      els.fx.append(tl);
      tl.animate([{ opacity: 0 }, { opacity: .7, offset: .15 }, { opacity: .7, offset: .6 }, { opacity: 0 }], { duration: 2800 }).addEventListener('finish', function () { tl.remove(); });
      var mx = (a.x + ex) / 2, my = (a.y + ey) / 2 - 36;
      launch(bubbleEl(plan, true), [
        { transform: tr(a.x, a.y, .5), opacity: 0, offset: 0 },
        { transform: tr(a.x, a.y, 1), opacity: 1, offset: .1, easing: 'cubic-bezier(.4,0,.2,1)' },
        { transform: tr(mx, my, 1.04), opacity: 1, offset: .33 },
        { transform: tr(ex, ey, 1), opacity: 1, offset: .55 },
        { transform: tr(ex, ey, 1), opacity: 1, offset: .86 },
        { transform: tr(ex, ey - 10, .96), opacity: 0, offset: 1 }], { duration: 2800 });
      toRec.root.animate([{ boxShadow: '0 0 0 0 rgba(120,160,255,0)' }, { boxShadow: '0 0 0 8px rgba(120,160,255,.45)', offset: .3 }, { boxShadow: '0 0 0 14px rgba(120,160,255,0)' }], { delay: 1500, duration: 800 });
      return;
    }
    var y = a.y - a.h / 2 - 30;
    var px = Math.max(110, Math.min(base.width - 110, a.x));
    if (reduced()) {
      launch(bubbleEl(plan, true), [{ transform: tr(px, y, 1), opacity: 0 }, { transform: tr(px, y, 1), opacity: 1, offset: .12 },
        { transform: tr(px, y, 1), opacity: 1, offset: .85 }, { transform: tr(px, y, 1), opacity: 0 }], { duration: 2400 });
      return;
    }
    launch(bubbleEl(plan, true), [{ transform: tr(px, y + 14, .6), opacity: 0 }, { transform: tr(px, y, 1), opacity: 1, offset: .15 },
      { transform: tr(px, y, 1), opacity: 1, offset: .8 }, { transform: tr(px, y - 14, .97), opacity: 0 }], { duration: 2400 });
    if (plan.kind === 'lost') fromRec.root.animate([{ transform: 'translateX(0)' }, { transform: 'translateX(-5px)', offset: .2 }, { transform: 'translateX(5px)', offset: .4 }, { transform: 'translateX(-3px)', offset: .6 }, { transform: 'none' }], { duration: 450 });
  }

  /* ingest + ticking */
  function reset() {
    state = core.emptyState(); queue = core.createQueue(); gotFirst = false;
    agentEls.forEach(function (r) { r.root.remove(); }); agentEls.clear();
    claimEls.forEach(function (r) { r.root.remove(); }); claimEls.clear(); ghosts.clear();
    els.feed.replaceChildren(); els.fx.replaceChildren(); inflight = 0; closeDetail();
  }
  function ingest(snap) {
    var first = !gotFirst; gotFirst = true;
    var r = core.applySnapshot(state, snap, Date.now());
    state = r.state;
    els.room.textContent = state.room;
    renderMembers(); renderClaims(r.lost); renderTasks(); feedAdd(r.fresh);
    var cut = first && !demo ? (+snap.now_ms || 0) - 15000 : -Infinity;   // don't replay old history on first load
    var plans = r.fresh.filter(function (e) { return e.ts_ms >= cut; }).map(function (e) { return core.planEvent(e, state.members); });
    core.queuePush(queue, plans);
    var last = r.fresh[r.fresh.length - 1];
    if (last && last.kind !== 'presence') els.sr.textContent = memberName(last.from) + ' ' + (core.LABELS[last.kind] || last.kind) + (last.text ? ': ' + last.text : '');
  }
  setInterval(function () {
    if (doc.hidden) { queue.items.length = 0; return; }
    var got = core.queueTake(queue, Date.now(), inflight, 5);
    if (got.length) playPlan(got[0]);
  }, 100);
  setInterval(function () {
    if (doc.hidden) return;
    renderClaims([]);
  }, 500);

  /* modes */
  function runDemo() {
    var extra = parseInt(params.get('agents') || '0', 10) || 0;
    var timer = 0;
    function cycle() {
      reset();
      var base = Date.now(), d = core.buildDemo(base, extra), i = 0, t0 = performance.now();
      demoDetails = d.details;
      setConn(true);
      (function step() {
        var el = performance.now() - t0;
        while (i < d.frames.length && d.frames[i].at <= el) ingest(d.frames[i++].snap);
        if (el >= d.length) { cycle(); return; }
        timer = setTimeout(step, 100);
      })();
    }
    void timer;
    cycle();
  }
  function runLive() {
    var failures = 0;
    function next() { setTimeout(poll, 1000); }
    function poll() {
      var ctl = new AbortController(), kill = setTimeout(function () { ctl.abort(); }, 4000);
      fetch('api/state?since=' + encodeURIComponent(String(state.cursor)), { cache: 'no-store', credentials: 'same-origin', signal: ctl.signal })
        .then(function (res) { if (!res.ok) throw new Error('http ' + res.status); return res.json(); })
        .then(function (snap) { clearTimeout(kill); failures = 0; setConn(true); ingest(snap); next(); })
        .catch(function () { clearTimeout(kill); failures++; setConn(false); next(); });
    }
    setConn(false); els.conn.textContent = 'connecting';
    poll();
  }
  syncEmpty();
  if (demo) runDemo(); else runLive();
})(typeof globalThis !== 'undefined' ? globalThis : this);
