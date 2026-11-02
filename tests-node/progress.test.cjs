'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { DatabaseSync } = require('node:sqlite');
const { Engine } = require('../runtime/engine.cjs');
const { Studio } = require('../runtime/studio.cjs');

test('activity pages backfill prior turns and merge live receipts without gaps', async (t) => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sa-progress-'));
  const engine = new Engine(dir, {}),
    studio = new Studio(engine, null, null, dir);
  t.after(async () => {
    await engine.close();
    await fs.rm(dir, { recursive: true, force: true });
  });
  const ticket = await engine.create({ cwd: dir, goal: 'investigate', lead: 'claude' }),
    job = engine.root(ticket);
  const reopened = new DatabaseSync(path.join(dir, 'hub.sqlite3'), { readOnly: true });
  const reader = new Studio({ db: reopened }, null, null, dir, { readOnly: true });
  t.after(() => {
    if (reopened.isOpen) reopened.close();
  });
  engine.run("UPDATE jobs SET state='running' WHERE id=?", job.id);
  const tool = (n) => ({
    type: 'assistant',
    message: { content: [{ type: 'tool_use', id: 'u-' + n, name: 'Read', input: { path: 'HIDDEN-PATH' } }] }
  });
  engine.log(job, 'native', {
    type: 'system',
    subtype: 'task_started',
    task_type: 'local_agent',
    task_id: 'sub1',
    tool_use_id: 'parent'
  });
  for (let i = 0; i < 210; i++) engine.log(job, 'native', tool(i));
  const original = await reader.snapshot({ ticket });
  assert.equal(original.nativeActivity.tasks[0].state, 'running');
  assert.ok(!JSON.stringify(original.nativeActivity).includes('HIDDEN-PATH'));
  const seqs = [];
  let before;
  do {
    const page = await reader.activityPage({ ticket, before });
    assert.ok(page.events.every((e) => e.agent === 'claude' && e.job === job.id && e.turn === 1));
    seqs.push(...page.events.map((e) => e.seq));
    before = page.more ? page.next : 0;
  } while (before);
  assert.equal(seqs.length, 212);
  assert.equal(new Set(seqs).size, 212);
  const expected = [];
  for (let i = 0; i < 240; i++)
    expected.push(engine.log(job, i % 2 ? 'native' : 'delta', i % 2 ? tool(i) : 'text'));
  const first = await reader.snapshot({ ticket, after: original.cursor });
  const second = await reader.snapshot({ ticket, after: first.cursor });
  assert.deepEqual(
    [...first.events, ...second.events].map((e) => e.seq),
    expected
  );
  engine.run("UPDATE jobs SET state='done' WHERE id=?", job.id);
  assert.equal((await reader.snapshot({ ticket })).nativeActivity.tasks[0].state, 'unknown');
  studio.planAdd({ ticket, body: 'review' });
  assert.equal((await reader.activityPage({ ticket })).events[0].payload.label, '已添加计划项');
  reopened.close();
});
