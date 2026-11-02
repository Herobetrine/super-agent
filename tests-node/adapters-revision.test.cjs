'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

test('catalog revisions observe declared files and database relocation without model discovery', (t) => {
  const temp = fs.realpathSync(os.tmpdir());
  const root = fs.mkdtempSync(path.join(temp, 'adapter-revision-'));
  const dir = (...parts) => path.join(root, ...parts);
  const env = {
    HOME: dir('home'),
    USERPROFILE: dir('home'),
    APPDATA: dir('roaming'),
    PATH: dir('bin'),
    CLAUDE_CONFIG_DIR: dir('claude'),
    DSH_HOME: dir('dsh'),
    SUPERAGENT_DSH_RUNTIME: dir('runtime'),
    CODEX_HOME: dir('codex'),
    SUPERAGENT_CODEX_PROFILE: 'special'
  };
  const old = Object.fromEntries(Object.keys(env).map((k) => [k, process.env[k]]));
  Object.assign(process.env, env);
  t.after(() => {
    for (const [k, v] of Object.entries(old)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    const target = fs.realpathSync(root);
    assert.equal(path.dirname(target), temp);
    assert.ok(path.basename(target).startsWith('adapter-revision-'));
    fs.rmSync(target, { recursive: true });
  });
  let forbidden = 0;
  const reject = () => {
    forbidden++;
    throw Error('revision attempted discovery');
  };
  // Import the adapter module after replacing these transports.
  for (const method of ['spawn', 'spawnSync', 'exec', 'execSync', 'execFile', 'execFileSync'])
    t.mock.method(require('node:child_process'), method, reject);
  t.mock.method(require('node:sqlite'), 'DatabaseSync', reject);
  t.mock.method(require('node:http'), 'get', reject);
  t.mock.method(require('node:https'), 'get', reject);
  t.mock.method(globalThis, 'fetch', reject);
  const realRead = fs.readFileSync;
  const opens = [];
  t.mock.method(fs, 'readFileSync', function (filename, ...args) {
    if (typeof filename === 'string' && path.resolve(filename).startsWith(root + path.sep)) {
      opens.push(path.basename(filename));
      if (
        [
          '.credentials.json',
          '.credentials.yaml',
          'auth.json',
          '.env',
          'cc-switch.db',
          'cc-switch.db-wal'
        ].includes(path.basename(filename))
      )
        reject();
    }
    return realRead.call(fs, filename, ...args);
  });
  const { catalogRevision } = require('../runtime/adapters/index.cjs');
  const set = (filename, contents) => {
    fs.mkdirSync(path.dirname(filename), { recursive: true });
    fs.writeFileSync(filename, contents);
  };
  function changed(agent, func) {
    const prev = catalogRevision(agent);
    assert.match(prev, /^[a-f0-9]{64}$/);
    func();
    const next = catalogRevision(agent);
    assert.notEqual(next, prev);
    assert.equal(catalogRevision(agent), next, 'unchanged sources keep the same revision');
  }
  function createModifyDelete(agent, filename) {
    changed(agent, () => set(filename, 'first'));
    changed(agent, () => fs.appendFileSync(filename, '-updated'));
    changed(agent, () => fs.unlinkSync(filename));
  }

  const base = dir('codex', 'config.toml'),
    selected = dir('codex', 'special.config.toml');
  set(base, 'model_catalog_json="基础.json"\n');
  createModifyDelete('codex', dir('codex', '基础.json'));
  changed('codex', () => set(selected, 'model_catalog_json="选择.json"\n'));
  createModifyDelete('codex', dir('codex', '选择.json'));
  const count = opens.length,
    active = catalogRevision('codex');
  set(dir('codex', '基础.json'), 'superseded');
  assert.equal(catalogRevision('codex'), active);
  assert.equal(opens.length, count, 'no repeated pointer-file reads while its metadata is unchanged');
  createModifyDelete('codex', dir('codex', 'models_cache.json'));
  createModifyDelete('codex', dir('codex', 'auth.json'));
  createModifyDelete('codex', dir('codex', 'alternate.config.toml'));

  const locator = dir('roaming', 'com.ccswitch.desktop', 'app_paths.json');
  const dbA = dir('源一', 'cc-switch.db'),
    dbB = dir('源二', 'cc-switch.db');
  set(locator, JSON.stringify({ app_config_dir_override: path.dirname(dbA) }));
  for (const agent of ['claude', 'antigravity', 'deepseek']) {
    createModifyDelete(agent, dbA);
    createModifyDelete(agent, dbA + '-wal');
  }
  changed('claude', () => set(locator, JSON.stringify({ app_config_dir_override: path.dirname(dbB) })));
  const rebased = catalogRevision('claude');
  set(dbA + '-wal', 'old origin');
  assert.equal(catalogRevision('claude'), rebased);
  createModifyDelete('claude', dbB + '-wal');

  for (const filename of ['settings.json', '.credentials.json'])
    createModifyDelete('claude', dir('claude', filename));
  createModifyDelete('claude', dir('home', '.cc-switch', 'live-state.json'));
  createModifyDelete('claude', dir('home', '.local/share/superagent/claude-route.json'));
  createModifyDelete('antigravity', dir('home', '.gemini/antigravity-cli/settings.json'));
  for (const f of [
    '.credentials.yaml',
    '.env',
    'settings.yaml',
    'profiles/desktop/cordis.patch.yml',
    'profiles/headless/cordis.patch.yml',
    'cordis.patch.yml'
  ])
    createModifyDelete('deepseek', dir('dsh', f));
  for (const f of [
    'dsh/package.json',
    'dsh/lib/bin.js',
    'dsh-base/cordis.patch.yml',
    'dsh-llm-pi-ai/lib/index.js',
    'dsh-llm-deepseek-api-key/lib/index.js'
  ])
    createModifyDelete('deepseek', dir('runtime', 'node_modules/@deepseek-ai', f));
  createModifyDelete('deepseek', dir('home', '.local/share/superagent/deepseek-default.json'));
  assert.equal(forbidden, 0);
});
