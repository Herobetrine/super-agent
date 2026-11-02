'use strict';
const { Native } = require('./native.cjs');
const { Studio } = require('./studio.cjs');
const { Extensions } = require('./extensions.cjs');
const { Engine } = require('./engine.cjs');
const adapters = require('./adapters/index.cjs');
const { join } = require('node:path');
const { homedir } = require('node:os');
const native = new Native();
const dataDir =
  process.env.SUPERAGENT_DATA || join(homedir(), '.local', 'share', 'superagent', 'control-room');
let engine,
  studio,
  closing = false;
const studioOps = new Set([
  'history',
  'snapshot',
  'jobsPage',
  'defaults',
  'saveDefaults',
  'saveRoutes',
  'catalog',
  'catalogRevision',
  'create',
  'files',
  'diff',
  'planAdd',
  'planToggle',
  'exportTo',
  'extensions',
  'extensionDefaults',
  'mcpSave',
  'mcpRemove',
  'mcpProbe',
  'skillPreview',
  'registerSkill',
  'forgetSkill'
]);
const engineOps = new Set(['cancel', 'follow', 'note', 'retry', 'decide']);

async function shutdown() {
  if (closing) return;
  closing = true;
  try {
    await engine?.close();
  } finally {
    // Signal after durable engine shutdown. The native owner can now release
    // its Windows job, which includes this broker and any orphaned CLI.
    if (process.connected) process.send({ closed: true }, () => native.close());
    else await native.close();
  }
}
process.on('disconnect', () => {
  void shutdown();
});
process.on('message', async (m) => {
  if (m.close) {
    await shutdown();
    return;
  }
  try {
    if (closing || !studio) throw new Error('调度台尚未就绪');
    let value;
    if (m.method === 'authorize') value = studio.authorize(m.params.cwd);
    else if (studioOps.has(m.method)) value = await studio[m.method](m.params);
    else if (engineOps.has(m.method)) value = await engine[m.method](m.params);
    else throw new Error('未知操作');
    if (process.connected) process.send({ id: m.id, value: value ?? null });
  } catch (e) {
    if (process.connected) process.send({ id: m.id, error: String(e.message) });
  }
});
(async () => {
  await native.call('claim', { directory: dataDir, pid: process.pid });
  engine = new Engine(dataDir, adapters, native);
  engine.extensions = new Extensions(engine);
  studio = new Studio(engine, native, adapters, dataDir);
  engine.start();
  process.send({ ready: true });
})().catch((error) => {
  if (process.connected)
    process.send({ fatal: error.message }, () => {
      void native.close().finally(() => process.exit(1));
    });
});
