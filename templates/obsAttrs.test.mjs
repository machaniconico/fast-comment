import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Comment templates that tag their root element with data-* attributes.
// ranking / goals / timer only show aggregates and are intentionally excluded.
const TEMPLATES = ['default', 'simple', 'bubble', 'ticker', 'donation', 'danmaku'];
const BEGIN = '  // ---- fc-attrs:begin ----';
const END = '  // ---- fc-attrs:end ----';
const ATTRS = ['data-service', 'data-user', 'data-owner', 'data-user-id', 'data-kind', 'data-role'];

const sources = Object.fromEntries(
  TEMPLATES.map((t) => [t, readFileSync(new URL(`./${t}/app.js`, import.meta.url), 'utf8').replace(/\r\n/g, '\n')]),
);

function block(source) {
  const start = source.indexOf(BEGIN);
  const end = source.indexOf(END);
  assert.ok(start >= 0 && end > start, 'attrs block markers present');
  return source.slice(start, end + END.length);
}

const fcApplyAttrs = new Function(`${block(sources.default)}\nreturn fcApplyAttrs;`)();

function fakeEl() {
  const attrs = new Map();
  const removed = [];
  return {
    attrs,
    removed,
    setAttribute: (k, v) => attrs.set(k, v),
    removeAttribute: (k) => {
      attrs.delete(k);
      removed.push(k);
    },
  };
}

const full = {
  platform: 'niconico',
  kind: 'superChat',
  author: { id: 'u1', name: '太郎', roles: { broadcaster: true, member: true } },
};

test('every bundled comment template carries an identical copy of the block', () => {
  const canonical = block(sources.default);
  for (const t of TEMPLATES) assert.equal(block(sources[t]), canonical, `${t} block matches default`);
});

test('full message sets all six attributes (niconico -> niconama)', () => {
  const el = fakeEl();
  fcApplyAttrs(el, full);
  assert.equal(el.attrs.get('data-service'), 'niconama');
  assert.equal(el.attrs.get('data-user'), '太郎');
  assert.equal(el.attrs.get('data-user-id'), 'u1');
  assert.equal(el.attrs.get('data-kind'), 'superchat');
  assert.equal(el.attrs.get('data-role'), 'broadcaster member');
  assert.equal(el.attrs.get('data-owner'), 'true');
});

test('twitch / youtube / x keep their platform value', () => {
  for (const p of ['twitch', 'youtube', 'x']) {
    const el = fakeEl();
    fcApplyAttrs(el, { ...full, platform: p });
    assert.equal(el.attrs.get('data-service'), p);
  }
});

test('message without author or roles does not throw and degrades', () => {
  const el = fakeEl();
  fcApplyAttrs(el, { platform: 'twitch' });
  assert.equal(el.attrs.get('data-kind'), 'normal');
  assert.equal(el.attrs.get('data-owner'), 'false');
  for (const a of ['data-user', 'data-user-id', 'data-role']) {
    assert.ok(el.removed.includes(a), `${a} removed`);
    assert.ok(!el.attrs.has(a));
  }
});

test('null message removes all six attributes', () => {
  const el = fakeEl();
  fcApplyAttrs(el, full);
  fcApplyAttrs(el, null);
  for (const a of ATTRS) assert.ok(el.removed.includes(a), `${a} removed`);
  assert.equal(el.attrs.size, 0);
});

test('reusing an element does not leave the previous role behind', () => {
  const el = fakeEl();
  fcApplyAttrs(el, { platform: 'twitch', author: { id: 'm', name: 'm', roles: { moderator: true } } });
  assert.equal(el.attrs.get('data-role'), 'moderator');
  fcApplyAttrs(el, { platform: 'twitch', author: { id: 'n', name: 'n', roles: {} } });
  assert.ok(!el.attrs.has('data-role'));
});

test('names with quotes and newlines are passed through verbatim', () => {
  const el = fakeEl();
  const name = 'a"b\nc<d>';
  fcApplyAttrs(el, { platform: 'twitch', author: { id: 'x', name } });
  assert.equal(el.attrs.get('data-user'), name);
});

test('each template calls fcApplyAttrs outside the block', () => {
  for (const t of TEMPLATES) {
    const outside = sources[t].replace(block(sources[t]), '');
    const n = outside.split('fcApplyAttrs(').length - 1;
    assert.ok(n >= (t === 'danmaku' ? 2 : 1), `${t}: ${n} calls`);
  }
  const ticker = sources.ticker;
  assert.ok(ticker.includes('fcApplyAttrs(tickerText'));
  assert.ok(ticker.includes('fcApplyAttrs(tickerText, null)'));
});
