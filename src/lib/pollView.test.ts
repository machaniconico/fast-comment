import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isWinner, pollRows, roundPercent, sector, timeLeft } from './pollView.ts';

test('rows preserve Rust percentages, tie flags and palette order', () => {
  const rows = pollRows([
    { label: 'A', votes: 1, percent: 50, leading: true },
    { label: 'B', votes: 1, percent: 50, leading: true },
  ]);
  assert.equal(rows[0].percent, 50);
  assert.equal(rows[1].start, 50);
  assert.notEqual(rows[0].color, rows[1].color);
  assert.equal(isWinner('closed', rows[0].leading), true);
  assert.equal(isWinner('closed', rows[1].leading), true);
  assert.equal(isWinner('open', rows[1].leading), false);
});

test('arcs and labels handle empty, full and fractional results', () => {
  assert.equal(sector(0, 0), '');
  assert.equal(sector(0, 100).match(/A /g)?.length, 2);
  assert.match(sector(10, 75), /A 45 45 0 1 1/);
  assert.equal(roundPercent(66.666), '67%');
  assert.equal(timeLeft(60999, 1000), '1:00');
  assert.equal(timeLeft(1000, 2000), '0:00');
});
