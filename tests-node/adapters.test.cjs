'use strict';
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  path = require('node:path'),
  os = require('node:os'),
  { Readable } = require('node:stream'),
  crypto = require('node:crypto');
const common = require('../runtime/adapters/common.cjs'),
  api = require('../runtime/adapters/index.cjs');
const tmp = (t) => {
  const root = fs.realpathSync(os.tmpdir());
  const p = fs.mkdtempSync(path.join(root, 'native-adapter-test-'));
  t.after(() => {
    const target = fs.realpathSync(p);
    assert.equal(path.dirname(target), root);
    assert.ok(path.basename(target).startsWith('native-adapter-test-'));
    fs.rmSync(target, { recursive: true });
  });
  return p;
};
test('UTF-8 transport preserves split code points, escape characters, NUL and final newline', async () => {
  const text = '😀\t汉\r\n\u0000 "尾" \n';
  const wire = Buffer.from(JSON.stringify({ text }) + '\r\n');
  const events = [];
  for await (const e of common.jsonLines(Readable.from(Array.from(wire, (x) => Buffer.from([x])))))
    events.push(e);
  assert.deepEqual(events, [{ text }]);
  const invalid = Buffer.concat([Buffer.from('{"text":"'), Buffer.from([0xc0, 0xaf]), Buffer.from('"}\n')]);
  await assert.rejects(async () => {
    for await (const _ of common.jsonLines(Readable.from([invalid]))) {
    }
  }, /UTF-8/);
});
test('explicit routes fail closed for stale providers, models and unbridged desktops', async () => {
  const cat = {
    default: { provider: 'p', model: 'm', mode: 'cli' },
    providers: [{ id: 'p', models: [{ id: 'm' }, { id: 'hidden', available: false }] }],
    modes: [
      { id: 'cli', available: true },
      { id: 'desktop', available: false }
    ]
  };
  assert.deepEqual(common.select(cat, null), cat.default);
  for (const x of [{ model: 'hidden' }, { provider: 'other' }, { mode: 'desktop' }])
    assert.throws(() => common.select(cat, { ...cat.default, ...x }));
  await assert.rejects(api.run('__proto__', {}), /未知/);
});
test('read, workspace and full grants are enforced before dispatch', async () => {
  const opts = { cwd: process.cwd(), writable: true };
  for (const agent of ['claude', 'antigravity'])
    await assert.rejects(common.scope(agent, { ...opts, profile: 'partial' }), /未获/);
  await assert.rejects(common.scope('deepseek', { ...opts, profile: 'readonly' }, true));
  await assert.rejects(
    common.scope('deepseek', { ...opts, profile: 'ask', approve: async () => false }, true)
  );
  assert.equal(
    await common.scope('deepseek', { ...opts, profile: 'ask', approve: async () => true }, true),
    'workspace-write'
  );
  await assert.rejects(common.scope('deepseek', { ...opts, writable: false, profile: 'auto' }, true));
  assert.equal(await common.scope('antigravity', { ...opts, profile: 'full' }), 'danger-full-access');
});
test('fresh TOML profile and catalog reload without exporting private gateway fields', async (t) => {
  const base = tmp(t),
    prev = process.env.CODEX_HOME,
    prevProfile = process.env.SUPERAGENT_CODEX_PROFILE;
  process.env.CODEX_HOME = base;
  process.env.SUPERAGENT_CODEX_PROFILE = 'test';
  t.after(() => {
    if (prev === undefined) delete process.env.CODEX_HOME;
    else process.env.CODEX_HOME = prev;
    if (prevProfile === undefined) delete process.env.SUPERAGENT_CODEX_PROFILE;
    else process.env.SUPERAGENT_CODEX_PROFILE = prevProfile;
  });
  fs.writeFileSync(
    path.join(base, 'config.toml'),
    'model_provider="gateway"\n[model_providers.gateway]\nname="local"\nbase_url="https://example.invalid/api"\nexperimental_bearer_token="fixture-private-token"\n'
  );
  for (const name of ['model-a', 'model-b']) {
    fs.writeFileSync(
      path.join(base, 'test.config.toml'),
      'model="' + name + '"\nmodel_catalog_json="models.json"\n'
    );
    fs.writeFileSync(
      path.join(base, 'models.json'),
      JSON.stringify({ models: [{ slug: name, visibility: 'list' }] })
    );
    const cat = await api.catalog('codex');
    assert.equal(cat.default.model, name);
    assert.equal(cat.default.provider, 'gateway');
    assert.deepEqual(
      cat.providers.find((p) => p.id === 'gateway').models.map((m) => m.id),
      [name]
    );
    assert.ok(!JSON.stringify(cat).includes('fixture-private-token'));
  }
});
test('CC Switch public IDs preserve the stored Python ID encoding across Unicode', () => {
  const id = crypto.createHash('sha256').update('["\\u4e2d", "b", ""]', 'utf8').digest('hex').slice(0, 24);
  assert.equal(common.fingerprint('ccs-', ['中', 'b', '']), 'ccs-' + id);
});
test('abort stops a native worker and its own descendant', { timeout: 10000 }, async (t) => {
  const dir = tmp(t),
    marker = path.join(dir, 'descendant-wrote');
  const grandchild = 'setTimeout(()=>require("node:fs").writeFileSync(process.argv[1],"escaped"),900)';
  const code =
    'const cp=require("node:child_process");const c=cp.spawn(process.execPath,["-e",' +
    JSON.stringify(grandchild) +
    ',process.argv[1]],{windowsHide:true,stdio:"ignore"});c.on("spawn",()=>console.log(JSON.stringify({ready:true})));setInterval(()=>{},999);';
  const abort = new AbortController();
  await assert.rejects(
    common.stream(process.execPath, ['-e', code, marker], '', { cwd: dir, signal: abort.signal }, (ev) => {
      if (ev.ready) abort.abort();
    })
  );
  await new Promise((r) => setTimeout(r, 1050));
  assert.ok(!fs.existsSync(marker));
});
