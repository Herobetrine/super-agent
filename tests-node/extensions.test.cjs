'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const { once } = require('node:events');
const { fork } = require('node:child_process');
const { Engine } = require('../runtime/engine.cjs');
const { Extensions } = require('../runtime/extensions.cjs');
const fixture = path.join(__dirname, 'fixtures/challenge-mcp.cjs');
test('skills, real stdio and HTTP MCP transports, exact consent and revocation', async (t) => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sa-ext-')),
    file = path.join(dir, 'secret');
  const engine = new Engine(dir, {}),
    ext = new Extensions(engine);
  t.after(async () => {
    await engine.close();
    await fs.rm(dir, { recursive: true, force: true });
  });
  await fs.writeFile(file, 'test-🧪-value');
  const folder = path.join(dir, 'selected-skill');
  await fs.mkdir(folder);
  await fs.writeFile(
    path.join(folder, 'SKILL.md'),
    '---\nname: selected-skill\ndescription: Example\n---\nKeep exact bytes.\n'
  );
  const skill = ext.import(folder);
  const config = { name: 'proof', command: process.execPath, args: [fixture, file], enabled: true };
  const server = ext.saveServer(config);
  assert.deepEqual(
    (await ext.probe(server)).map((r) => r.name),
    ['challenge']
  );
  const ticket = await engine.create({ cwd: dir, lead: 'deepseek', goal: 'demo', permission_profile: 'ask' });
  const job = engine.root(ticket).id;
  engine.run("UPDATE jobs SET state='running' WHERE id=?", job);
  ext.bind(ticket, ext.prepare({ skills: [skill], servers: [server] }, dir));
  await fs.writeFile(path.join(folder, 'SKILL.md'), 'changed');
  assert.match(ext.context(ticket, 'ask').skills[0].body, /Keep exact bytes/);
  assert.equal(ext.context(ticket, 'readonly').mcp.length, 0);
  const call = {
    ticket,
    job,
    turn: 1,
    logicalTurn: 1,
    profile: 'ask',
    signal: new AbortController().signal,
    approve: async () => false,
    command: { id: 'denied', server, tool: 'challenge', arguments: {} }
  };
  await ext.invoke(call);
  assert.equal(ext.results(job)[0].state, 'error');
  await assert.rejects(fs.stat(file + '.calls'), { code: 'ENOENT' });
  call.command.id = 'approved';
  call.approve = async () => true;
  assert.equal((await ext.invoke(call)).newReceipt, true);
  assert.equal((await ext.invoke(call)).newReceipt, false);
  assert.equal(await fs.readFile(file + '.calls', 'utf8'), '1\n');
  assert.equal(ext.results(job).at(-1).result.content[0].text, 'test-🧪-value');
  ext.saveServer({ ...config, id: server, enabled: false });
  ext.saveServer({ ...config, id: server });
  assert.equal(ext.context(ticket, 'ask').mcp.length, 0);
  const child = fork(fixture, [file, 'http'], { stdio: ['ignore', 'ignore', 'ignore', 'ipc'] });
  const exited = once(child, 'exit');
  t.after(async () => {
    child.kill();
    await exited;
  });
  const [{ port }] = await once(child, 'message');
  const remote = ext.saveServer({
    name: 'HTTP local',
    transport: 'http',
    url: 'http://127.0.0.1:' + port + '/mcp',
    enabled: true
  });
  assert.deepEqual(
    (await ext.probe(remote)).map((r) => r.name),
    ['challenge']
  );
  ext.bind(
    await engine.create({ cwd: dir, lead: 'codex', goal: 'http smoke' }),
    ext.prepare({ skills: [], servers: [remote] })
  );
  assert.throws(() =>
    ext.saveServer({ name: 'bad', transport: 'http', url: 'http://remote.invalid/mcp', enabled: true })
  );
});
