import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('./poll/app.js', import.meta.url), 'utf8');
const block = source.split('// ---- fc-poll:begin ----')[1].split('// ---- fc-poll:end ----')[0];
const view = runInNewContext(`${block}\n({ sector, roundPercent, isWinner, timeLeft })`);

test('pie sectors cover 0, a partial arc and a full turn', () => {
  assert.equal(view.sector(0, 0), '');
  assert.match(view.sector(0, 25), /A 45 45 0 0 1/);
  assert.match(view.sector(25, 75), /A 45 45 0 1 1/);
  assert.equal(view.sector(0, 100).match(/A /g).length, 2);
});

test('result labels use rounded percentages, ties and remaining time', () => {
  assert.equal(view.roundPercent(33.333), '33%');
  assert.equal(view.isWinner('closed', true), true);
  assert.equal(view.isWinner('open', true), false);
  assert.equal(view.isWinner('closed', false), false);
  assert.equal(view.timeLeft(61001, 1000), '1:01');
  assert.equal(view.timeLeft(1000, 1005), '0:00');
});
