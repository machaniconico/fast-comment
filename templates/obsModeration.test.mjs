import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Templates that show individual comments and must drop moderated ones from the DOM.
// ranking / goals / timer only show aggregates and are intentionally excluded.
const TEMPLATES = ['default', 'simple', 'bubble', 'ticker', 'donation', 'danmaku'];
const BEGIN = '  // ---- fc-moderation:begin ----';
const END = '  // ---- fc-moderation:end ----';

const sources = Object.fromEntries(
  TEMPLATES.map((t) => [t, readFileSync(new URL(`./${t}/app.js`, import.meta.url), 'utf8').replace(/\r\n/g, '\n')]),
);

function block(source) {
  const start = source.indexOf(BEGIN);
  const end = source.indexOf(END);
  assert.ok(start >= 0 && end > start, 'moderation block markers present');
  return source.slice(start, end + END.length);
}

/** Evaluate the shared block in isolation with an injected monotonic clock. */
function load(clock) {
  const factory = new Function(
    'performance',
    `${block(sources.default)}\nreturn { fcModFrameEvents, fcModMatches, fcModAnyMatch, fcModCreateLog, fcModRemoveFrom, FC_MOD_MAX, FC_MOD_TTL_MS };`,
  );
  return factory({ now: clock });
}

const msg = (id, authorId, timestampMs, over = {}) => ({
  id, platform: 'twitch', channel: 'ch', timestampMs, author: { id: authorId, name: authorId }, ...over,
});
const del = (messageId) => ({ type: 'messageDeleted', platform: 'twitch', channel: 'ch', messageId });
const user = (authorId, beforeMs = null, certain = true) =>
  ({ type: 'userCleared', platform: 'twitch', channel: 'ch', authorId, durationSecs: null, certain, beforeMs });
const clear = (beforeMs = null) => ({ type: 'chatCleared', platform: 'twitch', channel: 'ch', beforeMs });

test('every bundled comment template carries an identical copy of the block', () => {
  const canonical = block(sources.default);
  for (const t of TEMPLATES) assert.equal(block(sources[t]), canonical, t);
});

test('every template opts in, handles moderation frames before chat arrays, and checks late arrivals', () => {
  for (const t of TEMPLATES) {
    const s = sources[t];
    assert.match(s, /url\.searchParams\.set\('modEvents', '1'\)/, t);
    assert.match(s, /const modEvents = fcModFrameEvents\(batch\);\s+if \(modEvents\) \{\s+applyModeration\(modEvents\);\s+return;\s+\}\s+if \(!Array\.isArray\(batch\)\)/, t);
    if (t === 'donation') {
      // donation counts the total before the late-arrival check, so arrival order never changes it.
      assert.match(s, /function handleMessage\(msg\) \{[^}]*?recordDonation\(donation\);\s+renderTotals\(\);\s+if \(fcModLog\.matches\(msg\)\) return;/, t);
    } else {
      assert.match(s, /function handleMessage\(msg\) \{\n    if \(fcModLog\.matches\(msg\)\) return;/, t);
    }
    assert.match(s, /function applyModeration\(events\) \{\n(?:.*\n)?    fcModLog\.add\(events\);/, t);
  }
});

test('fcModFrameEvents only accepts {"moderation":[…]} frames', () => {
  const { fcModFrameEvents } = load(() => 0);
  assert.equal(fcModFrameEvents([{ id: 'chat' }]), null);
  assert.equal(fcModFrameEvents({ id: 'single chat' }), null);
  assert.equal(fcModFrameEvents(null), null);
  assert.equal(fcModFrameEvents({ moderation: 'oops' }), null);
  assert.deepEqual(fcModFrameEvents({ moderation: [del('a'), null, 3] }), [del('a')]);
});

test('fcModMatches: id / author / channel with beforeMs boundary, certain ignored', () => {
  const { fcModMatches } = load(() => 0);
  assert.equal(fcModMatches(del('a'), msg('a', 'u', 1)), true);
  assert.equal(fcModMatches(del('a'), msg('b', 'u', 1)), false);
  assert.equal(fcModMatches(del(''), msg('', 'u', 1)), false); // empty id never matches
  assert.equal(fcModMatches(del('a'), msg('a', 'u', 1, { platform: 'youtube' })), false);
  assert.equal(fcModMatches(del('a'), msg('a', 'u', 1, { channel: 'other' })), false);

  assert.equal(fcModMatches(user('u'), msg('x', 'u', 999)), true); // no boundary
  assert.equal(fcModMatches(user('u', 100), msg('x', 'u', 100)), true);
  assert.equal(fcModMatches(user('u', 100), msg('x', 'u', 101)), false);
  assert.equal(fcModMatches(user('u', null, false), msg('x', 'u', 1)), true); // uncertain still removes
  assert.equal(fcModMatches(user(''), msg('x', '', 1)), false);
  assert.equal(fcModMatches(user('u'), msg('x', 'v', 1)), false);

  assert.equal(fcModMatches(clear(), msg('x', 'u', 5)), true);
  assert.equal(fcModMatches(clear(5), msg('x', 'u', 5)), true);
  assert.equal(fcModMatches(clear(5), msg('x', 'u', 6)), false);
  assert.equal(fcModMatches(clear(5), msg('x', 'u', undefined)), false);
  assert.equal(fcModMatches({ type: 'future', platform: 'twitch', channel: 'ch' }, msg('x', 'u', 1)), false);
});

test('recent log: late-arriving comments are dropped, then expire per entry after 10 s', () => {
  let now = 1000;
  const { fcModCreateLog, FC_MOD_TTL_MS } = load(() => now);
  assert.equal(FC_MOD_TTL_MS, 10000);
  const log = fcModCreateLog();
  assert.equal(log.matches(msg('a', 'u', 1)), false);
  log.add([del('a')]);
  now = 6000;
  log.add([clear(50)]);
  assert.equal(log.matches(msg('a', 'u', 999)), true);
  assert.equal(log.matches(msg('b', 'u', 50)), true);
  assert.equal(log.matches(msg('c', 'u', 51)), false);
  now = 1000 + FC_MOD_TTL_MS + 1; // first entry expired, second still live
  assert.equal(log.matches(msg('a', 'u', 999)), false);
  assert.equal(log.matches(msg('b', 'u', 50)), true);
  assert.equal(log.size(), 1);
  now = 0; // clock earlier than the record → treated as expired, never kept forever
  assert.equal(log.matches(msg('b', 'u', 50)), false);
  assert.equal(log.size(), 0);
});

test('recent log keeps only the newest 512 entries', () => {
  const { fcModCreateLog, FC_MOD_MAX } = load(() => 0);
  assert.equal(FC_MOD_MAX, 512);
  const log = fcModCreateLog();
  log.add(Array.from({ length: FC_MOD_MAX + 1 }, (_, i) => del(`m${i}`)));
  assert.equal(log.size(), FC_MOD_MAX);
  assert.equal(log.matches(msg('m0', 'u', 1)), false);
  assert.equal(log.matches(msg(`m${FC_MOD_MAX}`, 'u', 1)), true);
});

test('fcModRemoveFrom removes matching elements (incl. coalesced ones) from the container', () => {
  const { fcModRemoveFrom } = load(() => 0);
  const container = { children: [] };
  const el = (msgs) => {
    const e = { __fcMsgs: msgs, parentNode: container };
    container.children.push(e);
    return e;
  };
  container.removeChild = (e) => {
    container.children = container.children.filter((c) => c !== e);
    e.parentNode = null;
  };
  const keep = el([msg('k', 'u2', 1)]);
  const gone = el([msg('a', 'u1', 1)]);
  const coalesced = el([msg('x', 'u3', 1), msg('y', 'u1', 2)]);
  el(undefined); // element without tracked messages is left alone
  const removed = fcModRemoveFrom(container, [user('u1')]);
  assert.deepEqual(removed, [gone, coalesced]);
  assert.equal(container.children.length, 2);
  assert.ok(container.children.includes(keep));
  assert.deepEqual(fcModRemoveFrom(null, [user('u1')]), []);
});
