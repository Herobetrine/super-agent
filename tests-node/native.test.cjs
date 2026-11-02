'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const { createHash } = require('node:crypto');
const { Native } = require('../runtime/native.cjs');

test('Rust search, exact binary snapshot and archived context stay within contracts', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'sa-native-test-'));
  const native = new Native();
  t.after(async () => {
    await native.close();
    await fs.rm(root, { recursive: true, force: true });
  });
  const cwd = path.join(root, 'project'),
    data = path.join(root, 'data');
  await fs.mkdir(cwd);
  await fs.mkdir(data);
  const name = '输入样本.bin',
    raw = Buffer.from([0, 10, 13, 255, 128]);
  await fs.writeFile(path.join(cwd, name), raw);
  await fs.mkdir(path.join(cwd, 'node_modules'));
  await fs.writeFile(path.join(cwd, 'node_modules', '伪造.bin'), raw);
  assert.deepEqual(await native.call('files', { cwd, q: '.bin' }), [name]);
  const snap = { project: cwd, names: [name], directory: path.join(data, 'objects') };
  const [file] = await native.call('snapshot', snap);
  assert.equal(file.sha256, createHash('sha256').update(raw).digest('hex'));
  await fs.writeFile(path.join(cwd, name), 'changed');
  assert.deepEqual(await fs.readFile(file.object_path), raw);
  await assert.rejects(native.call('snapshot', { ...snap, names: ['../private.txt'] }));
  const packet = {
    permission_profile: 'readonly',
    goal: '目标',
    user_updates: ['新指令'],
    reports: Array.from({ length: 128 }, (_, id) => ({ id, state: 'done', result: '成果\n🧪'.repeat(120) }))
  };
  const guide = '完整指导\n';
  const compact = await native.call('compact', { packet, guide, directory: data, max_bytes: 8192 });
  assert.ok(Buffer.byteLength(guide + '\n' + JSON.stringify(compact)) <= 8192);
  assert.equal(compact.reports.at(-1).id, 127);
  assert.equal(compact.permission_profile, 'readonly');
  assert.deepEqual(compact.user_updates, ['新指令']);
  const archived = await fs.readFile(compact.full_context.object_path);
  assert.deepEqual(JSON.parse(archived), packet);
  assert.equal(createHash('sha256').update(archived).digest('hex'), compact.full_context.sha256);
});
