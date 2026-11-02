'use strict';
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  path = require('node:path'),
  os = require('node:os'),
  crypto = require('node:crypto'),
  { performance } = require('node:perf_hooks'),
  { setTimeout: sleep } = require('node:timers/promises');
const { emitBatch } = require('../runtime/adapters/common.cjs');
test('a frame commits atomically when supported, otherwise awaits every ordered event', async () => {
  const frame = [
    ['native', { text: '⭐\n' }],
    ['delta', '⭐\n'],
    ['message.boundary', null]
  ];
  const fallback = [],
    batches = [];
  await emitBatch(async (k, v) => {
    await Promise.resolve();
    fallback.push([k, v]);
  }, frame);
  const emit = () => assert.fail('per-row callback should not run');
  emit.batch = async (rows) => {
    batches.push(rows);
  };
  await emitBatch(emit, frame);
  await emitBatch(emit, [['native', { final: true }]]);
  assert.deepEqual(fallback, frame);
  assert.deepEqual(batches, [frame, [['native', { final: true }]]]);
});
test(
  'benchmark individual commits and frame commits through the real Engine WAL journal',
  { skip: process.env.SUPERAGENT_NODE_BENCHMARK !== '1', timeout: 120000 },
  async () => {
    const { Engine } = require('../runtime/engine.cjs');
    const parent = path.join(os.homedir(), '.local/share/superagent/checks');
    fs.mkdirSync(parent, { recursive: true });
    const dir = fs.mkdtempSync(path.join(parent, 'node-frame-bench-'));
    const transcript = Array.from({ length: 250 }, (_, i) => {
      const text = '消息' + i + '😀\r\n尾 ';
      return [
        ['native', { sequence: i, text }],
        ['delta', text],
        ...(i % 5 === 4 ? [['message.boundary', null]] : [])
      ];
    });
    const measurements = [];
    for (const mode of ['separate', 'batch', 'batch', 'separate', 'separate', 'batch']) {
      let engine,
        tracking = false,
        count = 0,
        elapsed = 0;
      const selection = { mode: 'cli', provider: 'local-fixture', model: 'transport' };
      const adapters = {
        catalog: async () => ({
          default: selection,
          modes: [{ id: 'cli', available: true }],
          providers: [{ id: 'local-fixture', models: [{ id: 'transport' }] }]
        }),
        run: async (_agent, { emit }) => {
          const sink = mode === 'batch' ? emit : (k, v) => emit(k, v);
          const start = performance.now();
          tracking = true;
          try {
            for (const frame of transcript) await emitBatch(sink, frame);
          } finally {
            tracking = false;
            elapsed = performance.now() - start;
          }
          return { text: 'finished', sessionId: null };
        }
      };
      engine = new Engine(path.join(dir, String(measurements.length)), adapters);
      const tx = engine.tx.bind(engine);
      engine.tx = (fn, ...args) => {
        if (tracking) count++;
        return tx(fn, ...args);
      };
      try {
        const id = await engine.create({
          goal: 'bounded synthetic transport benchmark',
          cwd: dir,
          lead: 'codex',
          permission_profile: 'readonly',
          allow_lead_routing: false
        });
        engine.start();
        const deadline = Date.now() + 15000;
        while (Date.now() < deadline && engine.root(id).state !== 'done') await sleep(20);
        assert.equal(engine.root(id).state, 'done');
        const rows = engine.all(
          "SELECT kind,payload FROM events WHERE ticket=? AND kind IN ('native','delta','message.boundary') ORDER BY seq",
          id
        );
        const replay = rows.map((r) => [r.kind, JSON.parse(r.payload)]);
        assert.deepEqual(replay, transcript.flat());
        assert.equal(count, mode === 'batch' ? transcript.length : rows.length);
        measurements.push({
          mode,
          transactions: count,
          ms: elapsed,
          rows: rows.length,
          hash: crypto.createHash('sha256').update(JSON.stringify(replay)).digest('hex')
        });
      } finally {
        await engine.close();
      }
    }
    const summary = { frames: transcript.length, measurements };
    for (const m of ['separate', 'batch'])
      summary[m + 'MedianMs'] = measurements
        .filter((r) => r.mode === m)
        .map((r) => r.ms)
        .sort((a, b) => a - b)[1];
    fs.writeFileSync(path.join(dir, 'receipt.json'), JSON.stringify(summary, null, 2));
    console.log(dir, summary.separateMedianMs.toFixed(1), summary.batchMedianMs.toFixed(1));
  }
);
