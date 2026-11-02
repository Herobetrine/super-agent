'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { Engine } = require('../runtime/engine.cjs');
const { Studio } = require('../runtime/studio.cjs');

test('ticket preview and collaboration pages preserve every task address', async (t) => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sa-ticket-pages-'));
  const engine = new Engine(dir, {}),
    store = new Studio(engine, null, {}, dir);
  t.after(async () => {
    await engine.close();
    await fs.rm(dir, { recursive: true, force: true });
  });
  const ticket = await engine.create({ cwd: dir, goal: 'test project', lead: 'deepseek' });
  const root = engine.root(ticket);
  engine.tx(() => {
    const insert = engine.db.prepare(
      "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state) VALUES(?,?,'deepseek','codex',?,0,1,'done')"
    );
    for (let n = 0; n < 207; n++) insert.run(ticket, root.id, 'check ' + n);
  });
  const view = store.snapshot({ ticket });
  assert.equal(view.totalJobs, 208);
  assert.ok(view.jobs.length < view.totalJobs);
  const ids = new Set();
  let before;
  do {
    const page = store.jobsPage({ ticket, before });
    before = page.next;
    for (const { id } of page.jobs) {
      assert.ok(!ids.has(id));
      ids.add(id);
    }
    if (!page.more) break;
  } while (before);
  assert.equal(ids.size, 208);
  assert.ok(ids.has(root.id));
});
