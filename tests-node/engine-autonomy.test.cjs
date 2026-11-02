'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { setTimeout: sleep } = require('node:timers/promises');
const { Engine } = require('../runtime/engine.cjs');

const initial = { provider: 'source', mode: 'cli', model: 'base' };
const catalog = () => ({
  default: initial,
  modes: [{ id: 'cli', available: true }],
  providers: [
    { id: 'source', models: [{ id: 'base' }, { id: 'second' }, { id: 'removed', available: false }] },
    { id: 'foreign', models: [{ id: 'elsewhere' }] }
  ]
});
const cmd = (tag, obj) => tag + ' ' + JSON.stringify(obj);
const ended = cmd('SA_EXHAUSTED', {
  status: 'done',
  summary: 'project checked',
  checks: ['inspected files'],
  alternatives: ['no open request']
});
const done = cmd('SA_FINISH', { status: 'done', summary: 'verified milestone' });
function setup(t, fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-epoch-'));
  const calls = [];
  const adapters = {
    catalog,
    run: async (name, opts) => {
      const p = JSON.parse(opts.prompt.split('\n').at(-1));
      calls.push(p);
      return { text: await fn(p, opts), sessionId: 'mock-' + p.job };
    }
  };
  const f = { dir, calls, adapters, e: new Engine(dir, adapters, {}) };
  t.after(async () => {
    await f.e.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return f;
}
async function wait(f, condition, ms = 10000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const result = condition();
    if (result) return result;
    if (f.e.fault) throw Error(f.e.fault);
    f.e.step();
    await sleep(4);
  }
  throw Error(
    'timeout ' +
      JSON.stringify(
        f.e.all('SELECT id,turn,state,result FROM jobs').map((r) => ({ ...r, result: r.result?.slice(-220) }))
      )
  );
}
const create = (f, profile) =>
  f.e.create({ goal: 'fixture', cwd: f.dir, permission_profile: profile, lead: 'deepseek' });
const prove = (o) =>
  o.emit('native', {
    type: 'tool_result',
    callId: 'inspect',
    status: 'completed',
    result: 'checked fixture'
  });

test('new continuous goal must follow a real inspection; learning and review survive milestone recovery', async (t) => {
  let phase = 0,
    premature = 0,
    missingProof = 0;
  const later =
    cmd('SA_GOAL', { objective: 'second milestone', evidence: 'local requirement exists' }) +
    '\n' +
    cmd('SA_NEXT', { body: 'execute it' });
  const f = setup(t, async (p, o) => {
    if (p.autonomy.phase === 'work') {
      if (!phase++) return cmd('SA_LEARN', { lesson: 'remember exact bytes' }) + '\n' + done;
      assert.equal(p.objective, 'second milestone');
      prove(o);
      return done;
    }
    assert.equal(p.autonomy.learning, 'remember exact bytes');
    if (p.autonomy.milestone === 1) {
      if (premature++ === 0) return later;
      prove(o);
      return later;
    }
    if (!missingProof++) return ended;
    prove(o);
    return ended;
  });
  const tid = await create(f, 'auto');
  f.e.start();
  await wait(f, () => f.e.one("SELECT seq FROM events WHERE kind='auto.milestone'"));
  await f.e.close();
  f.e = new Engine(f.dir, f.adapters, {});
  f.e.start();
  await wait(f, () => f.e.root(tid).state === 'done');
  const counts = f.e.one(
    `SELECT sum(kind='auto.milestone') milestones,sum(kind='auto.exhausted') exhausted,
    sum(kind='auto.discovered') discovered FROM events WHERE ticket=?`,
    tid
  );
  assert.deepEqual({ ...counts }, { milestones: 2, exhausted: 1, discovered: 1 });
  assert.ok(premature >= 2 && missingProof >= 2);
});

test('continuous runs pass old budget; stop requires explicit resume even after restart', async (t) => {
  let entered = false;
  const f = setup(t, async (p, o) => {
    if (p.turn === 1) {
      entered = true;
      await new Promise((r) => o.signal.addEventListener('abort', r, { once: true }));
    }
    return done;
  });
  const id = await create(f, 'auto');
  f.e.run('UPDATE tickets SET auto_cycles=200 WHERE id=?', id);
  f.e.start();
  await wait(f, () => entered);
  f.e.cancel({ ticket: id });
  await wait(f, () => !f.e.active.size);
  await f.e.close();
  f.e = new Engine(f.dir, f.adapters, {});
  f.e.start();
  assert.equal(f.e.root(id).state, 'cancelled');
  assert.throws(() => f.e.follow({ ticket: id, body: 'inadvertent follow' }));
  f.e.follow({ ticket: id, body: 'repair explicitly', resume: true });
  assert.equal(f.e.root(id).state, 'queued');
});

test('interrupted turn cannot replay a writer or a delivered mailbox receipt', async (t) => {
  let stopping = true,
    childId,
    rootId;
  const text = '原样\r\n  ';
  let f;
  const send = cmd('SA_SEND', { id: 'once', to: 'claude', body: 'isolated', permission_profile: 'readonly' });
  f = setup(t, async (p, o) => {
    if (p.depth) {
      childId = p.job;
      return 'actual report';
    }
    rootId = p.job;
    if (p.autonomy.phase === 'discover') {
      prove(o);
      return ended;
    }
    if (stopping) {
      o.emit('delta', send + '\n');
      await new Promise((r) => o.signal.addEventListener('abort', r, { once: true }));
      return 'interrupted';
    }
    return send + '\n' + done;
  });
  const id = await create(f, 'auto');
  f.e.start();
  await wait(f, () => childId && f.e.one('SELECT state FROM jobs WHERE id=?', childId)?.state === 'done');
  const seq = f.e.note({ ticket: id, job: rootId, body: text });
  await f.e.close();
  stopping = false;
  f.e = new Engine(f.dir, f.adapters, {});
  f.e.start();
  await wait(f, () => f.e.root(id).state === 'done');
  assert.equal(f.calls.filter((p) => p.depth).length, 1);
  assert.equal(f.e.all("SELECT seq FROM events WHERE kind='send'").length, 1);
  assert.equal(f.e.notes(rootId).find((n) => n.seq === seq).body, text);
});

test('disabled models, provider injection and escalation are rejected before any child executes', async (t) => {
  const commands = new Map();
  const f = setup(t, async (p) => commands.get(p.ticket));
  for (const overrides of [
    { model: 'removed' },
    { model: 'elsewhere' },
    { permission_profile: 'full' },
    { provider: 'foreign' }
  ]) {
    const id = await create(f, 'ask');
    commands.set(id, cmd('SA_SEND', { id: 'x', to: 'claude', body: 'deny', ...overrides }));
  }
  f.e.start();
  await wait(f, () => [...commands.keys()].every((i) => f.e.root(i).state === 'failed'));
  assert.equal(f.calls.filter((p) => p.depth).length, 0);
});

test('auto job cannot hang on delegated manual approval; mailbox note wakes parent while a child is running', async (t) => {
  let noted = false,
    verdict,
    childEntered = false,
    release;
  const gate = new Promise((r) => (release = r));
  const f = setup(t, async (p, o) => {
    if (p.depth) {
      childEntered = true;
      verdict = await o.approve({ operation: 'edit' });
      await gate;
      return 'checked';
    }
    if (p.autonomy.phase === 'discover') {
      prove(o);
      return ended;
    }
    if (p.turn === 1)
      return cmd('SA_SEND', { id: 'limited', to: 'deepseek', body: 'read', permission_profile: 'ask' });
    if (p.notes.length && !noted) {
      noted = true;
      release();
    }
    return done;
  });
  const id = await create(f, 'auto');
  f.e.start();
  await wait(f, () => childEntered && f.e.root(id).state === 'waiting' && verdict !== undefined);
  f.e.note({ ticket: id, job: f.e.root(id).id, body: 'check progress now' });
  await wait(f, () => f.e.root(id).state === 'done');
  assert.equal(verdict, false);
  assert.equal(noted, true);
  assert.equal(f.e.all("SELECT seq FROM events WHERE kind='approval.unattended_decline'").length, 1);
});

test('MCP effects are final-only, bound by ticket, and require another turn to read new receipts', async (t) => {
  let invokes = 0;
  const receipts = [];
  const f = setup(t, async (p, o) => {
    assert.equal(p.project_extensions.skills[0].name, 'fixture');
    if (p.turn === 1) {
      const command = cmd('SA_TOOL', { id: 'calc', server: 'local', tool: 'sum', arguments: { x: 1 } });
      o.emit('delta', command + '\n');
      await sleep(50);
      assert.equal(invokes, 0);
      return command;
    }
    assert.deepEqual(p.mcp_results, receipts);
    return 'tool returned 2';
  });
  f.e.extensions = {
    prepare: async (x) => x,
    bind: () => {},
    context: () => ({
      skills: [{ name: 'fixture', body: 'test' }],
      mcp: [{ server: 'local', tools: [{ name: 'sum' }] }]
    }),
    results: () => receipts.slice(),
    async invoke(args) {
      assert.equal(args.command.id, 'calc');
      assert.equal(args.profile, 'full');
      assert.ok(Number.isSafeInteger(args.admission));
      invokes++;
      receipts.push({ id: 'calc', result: 2 });
      return { newReceipt: true };
    }
  };
  const id = await create(f, 'full');
  f.e.start();
  await wait(f, () => f.e.root(id).state === 'done');
  assert.equal(invokes, 1);
  assert.equal(f.e.root(id).turn, 2);
});

test('a readonly child cannot launder the ticket grant through a grandchild', async (t) => {
  const f = setup(t, (p, o) => {
    if (!p.depth && p.turn === 1)
      return cmd('SA_SEND', { id: 'narrow', to: 'claude', body: 'inspect', permission_profile: 'readonly' });
    if (p.depth === 1 && p.turn === 1)
      return cmd('SA_SEND', { id: 'pass', to: 'codex', body: 'second opinion' });
    if (p.depth === 2) {
      assert.equal(o.profile, 'readonly');
      assert.equal(o.writable, false);
      assert.equal(p.allow_lead_routing, false);
    }
    return 'verified';
  });
  const id = await create(f, 'full');
  f.e.start();
  await wait(f, () => f.e.root(id).state === 'done');
  assert.equal(f.calls.filter((p) => p.depth === 2).length, 1);
});

test('human redirect during directory discovery suppresses old sends and old completion', async (t) => {
  let enter, release;
  const entered = new Promise((r) => (enter = r)),
    gate = new Promise((r) => (release = r));
  const f = setup(t, async (p, o) => {
    if (p.turn === 1)
      return cmd('SA_SEND', { id: 'stale', to: 'claude', body: 'must not run' }) + '\n' + done;
    if (p.autonomy.phase === 'discover') {
      prove(o);
      return ended;
    }
    assert.deepEqual(p.user_updates, ['new objective']);
    return done;
  });
  f.adapters.catalog = async () => {
    enter();
    await gate;
    return catalog();
  };
  const id = await create(f, 'auto');
  f.e.start();
  f.e.step();
  await entered;
  f.e.follow({ ticket: id, body: 'new objective' });
  release();
  await wait(f, () => f.e.root(id).state === 'done');
  assert.equal(f.e.all('SELECT id FROM jobs WHERE parent IS NOT NULL').length, 0);
  assert.deepEqual(
    f.e.all("SELECT turn FROM events WHERE ticket=? AND kind='auto.milestone'", id).map((x) => x.turn),
    [2]
  );
});
