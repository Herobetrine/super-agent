'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { setTimeout: sleep } = require('node:timers/promises');
const { DatabaseSync } = require('node:sqlite');
const { Engine } = require('../runtime/engine.cjs');

const hash = (s) => crypto.createHash('sha256').update(s).digest('hex');
const route = { provider: 'test-local', model: 'default', mode: 'cli' };
function catalog() {
  return {
    default: route,
    legacy_defaults: { user: route },
    modes: [{ id: 'cli', available: true }],
    providers: [
      {
        id: 'test-local',
        models: [{ id: 'default' }, { id: 'review' }, { id: 'disabled', available: false }]
      },
      { id: 'other', models: [{ id: 'foreign' }] }
    ]
  };
}
function gate() {
  let resolve;
  const promise = new Promise((r) => {
    resolve = r;
  });
  return { promise, resolve };
}
async function until(fn, engine) {
  const end = Date.now() + 6000;
  while (Date.now() < end) {
    const value = fn();
    if (value) return value;
    if (engine?.fault) throw new Error(engine.fault);
    engine?.step();
    await sleep(5);
  }
  throw new Error('timed out ' + JSON.stringify(engine?.all('SELECT id,turn,state,result FROM jobs')));
}
function fixture(
  t,
  fn,
  native = {
    call() {
      throw new Error('unexpected native call');
    }
  }
) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-node-test-'));
  const project = path.join(dir, 'project'),
    data = path.join(dir, 'state');
  fs.mkdirSync(project);
  const calls = [];
  const adapters = {
    catalog,
    async run(name, o) {
      const packet = JSON.parse(o.prompt.split('\n').at(-1));
      calls.push({ name, packet, o });
      const text = await fn({ name, packet, o });
      return { text, sessionId: 'session-' + packet.job };
    }
  };
  const f = { engine: new Engine(data, adapters, native), dir, data, project, calls, adapters, native };
  t.after(async () => {
    await f.engine.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return f;
}
const wire = (tag, obj) => tag + ' ' + JSON.stringify(obj);
const send = (x = {}) =>
  wire('SA_SEND', { id: 'peer', to: 'deepseek', body: '核对  \r\n\t🧭', write: false, ...x });
const begin = async (f, fields = {}) => {
  const id = await f.engine.create({ goal: 'fixture', cwd: f.project, lead: 'codex', ...fields });
  f.engine.start();
  return id;
};
const finish = (f, id, state = 'done') => until(() => f.engine.root(id).state === state, f.engine);

test('streaming starts a peer; exact model/permission, text, evidence and hashes survive replay', async (t) => {
  const entered = gate(),
    body = '收到  \r\n😀e\u0301\u2028尾   ';
  const cmd = send({ model: 'review', permission_profile: 'readonly' });
  const f = fixture(t, async ({ packet: p, o }) => {
    if (p.depth) {
      entered.resolve();
      return body;
    }
    if (p.turn === 1) {
      o.emit('delta', cmd.slice(0, 9));
      o.emit('delta', cmd.slice(9));
      o.emit('message.boundary', null);
      await entered.promise;
      return cmd;
    }
    assert.equal(p.reports[0].result, body);
    return body;
  });
  const id = await begin(f, { permission_profile: 'full' });
  await finish(f, id);
  const child = f.calls.find((x) => x.packet.depth);
  assert.deepEqual(child.o.selection, { ...route, model: 'review' });
  assert.equal(child.o.writable, false);
  assert.equal(child.o.profile, 'readonly');
  assert.equal(child.packet.assignment, '核对  \r\n\t🧭');
  assert.equal(f.engine.all("SELECT seq FROM events WHERE kind='send'").length, 1);
  for (const e of f.engine.all('SELECT * FROM events ORDER BY seq')) assert.equal(e.sha256, hash(e.payload));
});

test('in-flight notes without native acceptance wake a child; host acceptance is idempotent', async (t) => {
  const enter = gate(),
    release = gate();
  const f = fixture(t, async ({ packet: p, o }) => {
    if (!p.depth) return p.turn === 1 ? send() : 'merged';
    if (p.turn === 1) {
      enter.resolve();
      await release.promise;
      const notes = o.pendingNotes();
      assert.equal(notes.length, 2);
      o.acceptNotes([notes[0].seq]);
      o.acceptNotes([notes[0].seq]);
      assert.deepEqual(
        o.pendingNotes().map((n) => n.body),
        ['after']
      );
      return 'first';
    }
    assert.deepEqual(
      p.notes.map((n) => n.body),
      ['during', 'after']
    );
    return 'second';
  });
  const id = await begin(f);
  await until(() => f.calls.some((x) => x.packet.depth), f.engine);
  await enter.promise;
  const job = f.engine.one('SELECT id FROM jobs WHERE parent IS NOT NULL').id;
  f.engine.note({ ticket: id, job, body: 'during' });
  f.engine.note({ ticket: id, job, body: 'after' });
  assert.throws(() => f.engine.note({ ticket: id + 1, job, body: 'wrong ticket' }));
  release.resolve();
  await finish(f, id);
  assert.equal(f.engine.all("SELECT * FROM events WHERE kind='note.accepted'").length, 1);
  assert.equal(f.engine.one('SELECT turn FROM jobs WHERE id=?', job).turn, 2);
});

test('SA_NOTE exact body and receipt do not duplicate on final replay', async (t) => {
  const f = fixture(t, async ({ packet: p, o }) => {
    if (!p.depth) return p.turn === 1 ? send() : 'read peer note';
    const line = wire('SA_NOTE', {
      id: 'n',
      job: p.roster.find((r) => r.parent === null).job,
      body: 'n\r\n '
    });
    o.emit('delta', line + '\n');
    return line;
  });
  const id = await begin(f);
  await finish(f, id);
  const rows = f.engine.all("SELECT * FROM events WHERE kind='note'");
  assert.equal(rows.length, 1);
  assert.equal(JSON.parse(rows[0].payload).sha256, hash('n\r\n '));
});

test('approval belongs to its live turn; cancellation rejects late writes and expires the dialog', async (t) => {
  let verdict = null;
  const f = fixture(t, async ({ o }) => {
    verdict = await o.approve({ action: 'edit', scope: 'workspace' });
    o.emit('delta', send() + '\n');
    return 'finished';
  });
  const id = await begin(f, { permission_profile: 'ask' });
  const row = await until(() => f.engine.one("SELECT id FROM approvals WHERE decision='pending'"), f.engine);
  f.engine.cancel({ ticket: id });
  await until(() => verdict !== null, f.engine);
  assert.equal(verdict, false);
  assert.throws(() => f.engine.decide({ id: row.id, allow: true }));
  assert.equal(f.engine.root(id).state, 'cancelled');
  assert.equal(f.engine.all('SELECT id FROM jobs').length, 1);
});

test('a pending writer approval cannot be accepted by a second turn; allow once is delivered', async (t) => {
  let calls = 0;
  const f = fixture(t, async ({ o }) => {
    assert.equal(await o.approve({ cmd: 'safe fixture' }), true);
    calls++;
    return 'approved';
  });
  const id = await begin(f, { permission_profile: 'partial' });
  const row = await until(() => f.engine.one("SELECT id FROM approvals WHERE decision='pending'"), f.engine);
  f.engine.decide({ id: row.id, allow: true });
  assert.throws(() => f.engine.decide({ id: row.id, allow: true }));
  await finish(f, id);
  assert.equal(calls, 1);
});

test('declared writers serialize across tickets', async (t) => {
  let writers = 0,
    peak = 0;
  const first = gate();
  const f = fixture(t, async ({ o }) => {
    assert.equal(o.writable, true);
    peak = Math.max(peak, ++writers);
    await first.promise;
    writers--;
    return 'saved';
  });
  const a = await begin(f, { permission_profile: 'partial' });
  const b = await f.engine.create({
    goal: 'second project',
    cwd: f.project,
    lead: 'deepseek',
    permission_profile: 'partial'
  });
  await until(() => writers === 1, f.engine);
  f.engine.step();
  assert.equal(f.calls.length, 1);
  first.resolve();
  await finish(f, a);
  await finish(f, b);
  assert.equal(peak, 1);
});

for (const profile of ['partial', 'auto']) {
  test(
    profile + ' ticket admits 80 same-harness readers and one writer; cancellation reaches all 81',
    async (t) => {
      const readers = 80;
      const child = (id, write) => send({ id, to: 'deepseek', body: id, write });
      const cmds = [
        ...Array.from({ length: readers }, (_, i) => child('reader-' + i, false)),
        child('writer-a', true),
        child('writer-b', true)
      ];
      let runningReaders = 0,
        runningWriters = 0,
        aborted = 0;
      const f = fixture(t, async ({ packet: p, o }) => {
        if (!p.depth) return cmds.join('\n');
        if (p.assignment.startsWith('reader-')) runningReaders++;
        else runningWriters++;
        await new Promise((resolve) => o.signal.addEventListener('abort', resolve, { once: true }));
        aborted++;
        return 'late result';
      });
      const id = await begin(f, { permission_profile: profile });
      await until(() => runningReaders === readers && runningWriters === 1, f.engine);
      assert.equal(f.engine.active.size, 81);
      const before = f.engine.all('SELECT state,count(*) AS n FROM jobs WHERE ticket=? GROUP BY state', id);
      assert.deepEqual(Object.fromEntries(before.map((r) => [r.state, r.n])), {
        running: 81,
        waiting: 1,
        queued: 1
      });
      f.engine.cancel({ ticket: id });
      await until(() => !f.engine.active.size, f.engine);
      assert.equal(aborted, 81);
      assert.equal(
        f.engine.one("SELECT count(*) AS n FROM jobs WHERE ticket=? AND state='cancelled'", id).n,
        83
      );
      assert.equal(runningWriters, 1);
    }
  );
}

test('nine delegated levels of the same harness return through the complete chain', async (t) => {
  const f = fixture(t, ({ packet: p }) => {
    if (p.turn === 1 && p.depth < 9) return send({ body: 'inspect level ' + (p.depth + 1) });
    if (p.depth < 9) assert.equal(p.reports[0].result, 'verified ' + (p.depth + 1));
    return 'verified ' + p.depth;
  });
  const id = await begin(f);
  await finish(f, id);
  assert.equal(f.engine.root(id).result, 'verified 0');
  assert.equal(f.engine.one("SELECT count(*) AS n FROM jobs WHERE ticket=? AND state='done'", id).n, 10);
});

test('ordinary ticket archives oversized inputs with Rust and the adapter can verify the original', async (t) => {
  const goal = '大'.repeat(800000);
  let observed = false;
  const f = fixture(
    t,
    ({ packet: p, o }) => {
      assert.ok(Buffer.byteLength(o.prompt) <= 6 * 1024 * 1024);
      const bytes = fs.readFileSync(p.full_context.object_path);
      assert.equal(hash(bytes), p.full_context.sha256);
      assert.equal(JSON.parse(bytes).goal, goal);
      observed = true;
      return 'verified';
    },
    null
  );
  const id = await begin(f, { goal });
  await finish(f, id);
  assert.ok(observed);
});

test('root snapshot delivered byte-for-byte and plan exposed; cancellation during child snapshot is side-effect free', async (t) => {
  const admitted = gate(),
    release = gate();
  let copied = 0;
  const helper = {
    async call(method, params) {
      assert.equal(method, 'snapshot');
      if (++copied === 2) {
        admitted.resolve();
        await release.promise;
      }
      const n = params.names[0],
        bin = fs.readFileSync(path.join(params.project, n)),
        sha = hash(bin);
      fs.mkdirSync(params.directory, { recursive: true });
      const dest = path.join(params.directory, sha);
      fs.writeFileSync(dest, bin);
      return [{ name: n, bytes: bin.length, sha256: sha, object_path: dest }];
    }
  };
  const f = fixture(
    t,
    async ({ packet: p, o }) => {
      assert.equal(p.files[0].bytes, 4);
      assert.deepEqual(fs.readFileSync(p.files[0].object_path), Buffer.from([0, 255, 13, 10]));
      assert.equal(p.plan[0].body, 'operator plan');
      o.emit('delta', send({ files: ['image.bin'] }) + '\n');
      await release.promise;
      return 'no child';
    },
    helper
  );
  fs.writeFileSync(path.join(f.project, 'image.bin'), Buffer.from([0, 255, 13, 10]));
  const id = await f.engine.create({ goal: 'attachments', cwd: f.project, files: ['image.bin'] });
  f.engine.db.exec(
    'CREATE TABLE studio_plan(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,body TEXT NOT NULL,done INTEGER NOT NULL)'
  );
  f.engine.run("INSERT INTO studio_plan VALUES(1,?,'operator plan',0)", id);
  f.engine.start();
  await until(() => copied === 2, f.engine);
  await admitted.promise;
  f.engine.cancel({ ticket: id });
  release.resolve();
  await until(() => f.engine.active.size === 0, f.engine);
  assert.equal(f.engine.all('SELECT id FROM jobs').length, 1);
  await assert.rejects(f.engine.create({ goal: 'escape', cwd: f.project, files: ['../secret'] }));
});

test('legacy schema migration is non-destructive and recovery only runs after start', async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-old-'));
  const old = new DatabaseSync(path.join(dir, 'hub.sqlite3'));
  old.exec(
    'CREATE TABLE tickets(id INTEGER PRIMARY KEY,goal TEXT,cwd TEXT,writes INTEGER,lead TEXT,created REAL);'
  );
  old.prepare('INSERT INTO tickets VALUES(1,?,?,0,?,0)').run('retained', dir, 'codex');
  old.close();
  const e = new Engine(dir, { catalog, run: async () => ({ text: 'unused' }) }, {});
  t.after(async () => {
    await e.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  assert.deepEqual(
    { ...e.one('SELECT goal,permission_profile,allow_lead_routing FROM tickets') },
    { goal: 'retained', permission_profile: 'legacy', allow_lead_routing: 0 }
  );
  e.run("INSERT INTO jobs(ticket,agent,body,writing,depth,state) VALUES(1,'codex','saved',0,0,'running')");
  e.run("INSERT INTO approvals(ticket,job,turn,action) VALUES(1,1,1,'{}')");
  assert.equal(e.root(1).state, 'running');
  e.start();
  assert.equal(e.root(1).state, 'interrupted');
  assert.equal(e.one('SELECT decision FROM approvals').decision, 'expired');
});
