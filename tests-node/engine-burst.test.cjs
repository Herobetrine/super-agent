'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { setTimeout: sleep } = require('node:timers/promises');
const { performance } = require('node:perf_hooks');
const { Engine } = require('../runtime/engine.cjs');

test('1000 native+delta frame pairs remain byte-exact after durable batched acknowledgement', async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-burst-'));
  const selection = { provider: 'local', mode: 'cli', model: 'benchmark' };
  const catalog = () => ({
    default: selection,
    providers: [{ id: 'local', models: [{ id: 'benchmark' }] }],
    modes: [{ id: 'cli', available: true }]
  });
  let n = 0;
  const e = new Engine(
    dir,
    {
      catalog,
      async run(_, opts) {
        const before = performance.now();
        const pair = (i) => [
          ['native', { type: 'fixture', chunk: '字符😀 ' + i }],
          ['delta', '字符😀 ' + i + '\r\n']
        ];
        for (let i = 0; i < 1000; ) {
          // The first run uses the per-event callback. The second groups each
          // incoming native frame with its projection, as the providers can do.
          if (n === 0) {
            for (const [kind, data] of pair(i++)) await opts.emit(kind, data);
          } else if (n === 1) await opts.emit.batch(pair(i++));
          else {
            const burst = [];
            for (let j = 0; j < 32 && i < 1000; j++) burst.push(...pair(i++));
            await opts.emit.batch(burst);
          }
        }
        const duration = performance.now() - before;
        t.diagnostic(
          'WAL/FULL, 2,000 rows; ' +
            ['individual', 'frame-pair', 'batch-of-32-frames'][n++] +
            ': ' +
            duration.toFixed(1) +
            ' ms, ' +
            Math.round(2000000 / duration) +
            ' events/s'
        );
        return { text: 'ok', sessionId: null };
      }
    },
    {}
  );
  t.after(async () => {
    await e.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  e.start();
  for (let run = 0; run < 3; run++) {
    const ticket = await e.create({ goal: 'throughput sample', cwd: dir });
    const deadline = Date.now() + 60000;
    while (e.root(ticket).state !== 'done' && Date.now() < deadline) {
      e.step();
      await sleep(10);
    }
    assert.equal(e.root(ticket).state, 'done');
    const records = e.all(
      "SELECT kind,payload,sha256 FROM events WHERE ticket=? AND kind IN ('native','delta') ORDER BY seq",
      ticket
    );
    assert.equal(records.length, 2000);
    for (let i = 0; i < 1000; i++) {
      assert.deepEqual(JSON.parse(records[2 * i].payload), { type: 'fixture', chunk: '字符😀 ' + i });
      assert.equal(JSON.parse(records[2 * i + 1].payload), '字符😀 ' + i + '\r\n');
    }
    for (const r of records)
      assert.equal(crypto.createHash('sha256').update(r.payload).digest('hex'), r.sha256);
  }
  assert.equal(e.one('PRAGMA synchronous').synchronous, 2);
  assert.equal(e.one('PRAGMA journal_mode').journal_mode, 'wal');
});
