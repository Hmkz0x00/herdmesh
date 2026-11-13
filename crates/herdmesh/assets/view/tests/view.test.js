'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const core = require('../view.js');

const snap = (over) => Object.assign({ room: 'r', you: 'a/alice', now_ms: 100000, cursor: 1, members: [], leases: [], tasks: [], events: [] }, over);
const ev = (id, lamport, kind, extra) => Object.assign({ id, lamport, ts_ms: 1000 + lamport, kind, from: 'a/alice', to: null, text: '', ref: null }, extra);

test('formatters', () => {
  assert.equal(core.fmtCountdown(0), '0:00');
  assert.equal(core.fmtCountdown(61000), '1:01');
  assert.equal(core.fmtCountdown(1), '0:01');
  assert.equal(core.fmtAge(1000), 'now');
  assert.equal(core.fmtAge(125000), '2m');
  assert.equal(core.leaseFraction(5000, 10000), 0.5);
  assert.equal(core.leaseFraction(-1, 10000), 0);
  assert.equal(core.clip('abcdef', 4), 'abc\u2026');
  assert.equal(core.shortName('3d/alice'), 'alice');
  assert.equal(core.hueFor('x'), core.hueFor('x'));
});

test('applySnapshot orders by lamport, dedupes, never mutates prev', () => {
  const s0 = core.emptyState();
  const r1 = core.applySnapshot(s0, snap({ events: [ev('b', 2, 'ask'), ev('a', 1, 'join')] }), 0);
  assert.deepEqual(r1.fresh.map((e) => e.id), ['a', 'b']);
  assert.equal(s0.feed.length, 0);
  const r2 = core.applySnapshot(r1.state, snap({ events: [ev('b', 2, 'ask'), ev('c', 3, 'answer')] }), 0);
  assert.deepEqual(r2.fresh.map((e) => e.id), ['c']);
  assert.deepEqual(r2.state.feed.map((e) => e.id), ['c', 'b', 'a']);
});

test('applySnapshot normalises untrusted fields and tolerates junk', () => {
  const r = core.applySnapshot(core.emptyState(), snap({
    members: [{ id: '__proto__', name: 'x'.repeat(500), status: 'evil', role: 'root' }, null, {}],
    events: [null, { id: 1 }, ev('z', 1, 'note', { text: 'y'.repeat(999) })] }), 0);
  const m = r.state.members.get('__proto__');
  assert.equal(m.status, 'unknown'); assert.equal(m.role, 'member'); assert.ok(m.name.length <= 40);
  assert.ok(r.fresh.every((e) => e.text.length <= 200));
  assert.equal(core.applySnapshot(r.state, null, 0).state, r.state);
});

test('clock offset follows server now_ms', () => {
  const r = core.applySnapshot(core.emptyState(), snap({ now_ms: 5000 }), 1000);
  assert.equal(r.state.offset, 4000);
});

test('lost claim: vanished without release is reported, released is not', () => {
  const lease = { id: 'L1', paths: ['src/**'], holder: 'a/alice', state: 'granted', expires_ms: 100500 };
  const s1 = core.applySnapshot(core.emptyState(), snap({ leases: [lease] }), 0).state;
  const lostR = core.applySnapshot(s1, snap({ leases: [] }), 0);
  assert.equal(lostR.lost.length, 1);
  assert.equal(lostR.lost[0].reason, 'expired');
  assert.ok(lostR.fresh.some((e) => e.kind === 'lost' && /lost src/.test(e.text)));
  const relR = core.applySnapshot(s1, snap({ leases: [], events: [ev('r1', 5, 'release', { ref: 'L1' })] }), 0);
  assert.equal(relR.lost.length, 0);
  const far = Object.assign({}, lease, { expires_ms: 900000 });
  const s2 = core.applySnapshot(core.emptyState(), snap({ leases: [far] }), 0).state;
  assert.equal(core.applySnapshot(s2, snap({ leases: [] }), 0).lost[0].reason, 'revoked');
});

test('lease total remembered from first sighting', () => {
  const l = { id: 'L', paths: ['p'], holder: 'a/alice', state: 'granted', expires_ms: 130000 };
  const s1 = core.applySnapshot(core.emptyState(), snap({ leases: [l] }), 0).state;
  assert.equal(s1.leaseTotal.L, 30000);
  const s2 = core.applySnapshot(s1, snap({ now_ms: 110000, leases: [l] }), 10000).state;
  assert.equal(s2.leaseTotal.L, 30000);
});

test('planEvent picks travel for ask/answer/handoff between known members', () => {
  const members = new Map([['a/alice', { name: 'alice' }], ['b/bob', { name: 'bob' }]]);
  const p = core.planEvent(ev('1', 1, 'ask', { to: 'b/bob', text: 'hi' }), members);
  assert.equal(p.motion, 'travel'); assert.equal(p.toName, 'bob');
  assert.equal(core.planEvent(ev('2', 2, 'ask', { to: 'ghost/x' }), members).motion, 'pop');
  assert.equal(core.planEvent(ev('3', 3, 'claim'), members).motion, 'pop');
  assert.equal(core.planEvent(ev('4', 4, 'presence'), members).motion, 'none');
  assert.equal(core.planEvent(ev('5', 5, 'note', { from: 'nobody/x' }), members).motion, 'none');
});

test('queue paces starts, speeds up with backlog, trims pops first', () => {
  const q = core.createQueue();
  const mk = (i, motion) => ({ id: 'p' + i, motion: motion || 'pop' });
  core.queuePush(q, [mk(1), mk(2), { id: 'n', motion: 'none' }]);
  assert.equal(q.items.length, 2);
  assert.equal(core.queueTake(q, 1000, 0).length, 1);
  assert.equal(core.queueTake(q, 1100, 0).length, 0);
  assert.equal(core.queueTake(q, 1000 + core.queueGap(1), 0).length, 1);
  assert.ok(core.queueGap(20) < core.queueGap(8) && core.queueGap(8) < core.queueGap(1));
  const q2 = core.createQueue();
  core.queuePush(q2, [mk('t', 'travel')]);
  core.queuePush(q2, Array.from({ length: 40 }, (_, i) => mk(i)));
  assert.equal(q2.items.length, 30);
  assert.ok(q2.items.some((p) => p.motion === 'travel'));
  assert.equal(core.queueTake(core.createQueue(), 0, 0).length, 0);
  const q3 = core.createQueue(); core.queuePush(q3, [mk(1)]);
  assert.equal(core.queueTake(q3, 0, 5).length, 0);
});

test('demo script covers every required beat and replays cleanly', () => {
  const d = core.buildDemo(1700000000000, 0);
  let st = core.emptyState(); const kinds = new Set(); let lost = 0;
  for (const f of d.frames) {
    const r = core.applySnapshot(st, f.snap, f.snap.now_ms); st = r.state;
    r.fresh.forEach((e) => kinds.add(e.kind)); lost += r.lost.length;
  }
  for (const k of ['join', 'ask', 'answer', 'claim', 'lost', 'task_add', 'task_update', 'handoff', 'decide', 'release']) assert.ok(kinds.has(k), k);
  assert.equal(lost, 1);
  assert.ok(d.length > d.frames[d.frames.length - 1].at);
  assert.equal(core.buildDemo(0, 6).frames[0].snap.members.length, 7);
});

test('shortId / eventUrl / feedRow', () => {
  assert.equal(core.shortId('ab12cd34ef567890'), 'ab12cd');
  assert.equal(core.shortId(null), '');
  assert.equal(core.eventUrl('a/b c'), 'api/event?id=a%2Fb%20c');
  const members = new Map([['a/alice', { name: 'alice', local: true }], ['b/bob', { name: 'bob', local: false }]]);
  const row = core.feedRow({ id: 'ab12cd34ef', kind: 'ask', from: 'a/alice', to: 'b/bob', text: 'hi', ts_ms: 1.7e12, full: false }, members);
  assert.deepEqual([row.shortId, row.fromName, row.toName, row.fromLocal, row.toLocal, row.truncated, row.label], ['ab12cd', 'alice', 'bob', true, false, true, 'ask']);
  const solo = core.feedRow({ id: 'x', kind: 'note', from: 'zz/ghost', to: null, text: '', ts_ms: 0, full: true }, members);
  assert.equal(solo.fromName, 'ghost'); assert.equal(solo.toName, ''); assert.equal(solo.fromLocal, null); assert.equal(solo.truncated, false);
});

test('full flag survives applySnapshot; absent means not full', () => {
  const r = core.applySnapshot(core.emptyState(), snap({ events: [ev('a', 1, 'ask', { full: true }), ev('b', 2, 'ask')] }), 0);
  assert.deepEqual(r.fresh.map((e) => e.full), [true, false]);
});

test('detailFromEvent / parseDetail', () => {
  const d = core.detailFromEvent({ id: 'i', kind: 'ask', from: 'a', to: 'b', ts_ms: 5, text: 'short', ref: 't-3', full: false });
  assert.deepEqual([d.complete, d.refs], [false, ['t-3']]);
  assert.equal(core.parseDetail(null), null);
  assert.equal(core.parseDetail({ kind: 'ask' }), null);
  const p = core.parseDetail({ id: 5, kind: 'ask', from: 'a', to: null, ts_ms: '7', text: 'full text', refs: ['x', 'y'] });
  assert.deepEqual([p.id, p.ts_ms, p.text, p.refs, p.complete, p.to], ['5', 7, 'full text', ['x', 'y'], true, null]);
  assert.equal(core.parseDetail({ id: 1, text: 'x'.repeat(50000) }).text.length, 20000);
  assert.deepEqual(core.parseDetail({ id: 1, refs: 'nope' }).refs, []);
});

test('demo: shortened events have scripted full texts keyed by event id', () => {
  const d = core.buildDemo(1.7e12, 0);
  let st = core.emptyState(); const short = [];
  for (const f of d.frames) { const r = core.applySnapshot(st, f.snap, f.snap.now_ms); st = r.state; r.fresh.forEach((e) => { if (!e.full && e.kind !== 'lost') short.push(e); }); }
  assert.ok(short.length >= 3);
  for (const e of short) { assert.ok(d.details[e.id].text.length > e.text.length); assert.match(e.id, /^[0-9a-f]{16}$/); }
});
