'use strict';
const { DatabaseSync } = require('node:sqlite');
const path = require('node:path');
const os = require('node:os');
const { Studio } = require('./studio.cjs');
const ticket = Number(process.argv[2]);
const dir = process.env.SUPERAGENT_DATA || path.join(os.homedir(), '.local/share/superagent/control-room');
try {
  if (!Number.isSafeInteger(ticket) || ticket < 1) throw Error('无效的观察工单');
  const db = new DatabaseSync(path.join(dir, 'hub.sqlite3'), { readOnly: true });
  if (!db.prepare('SELECT 1 FROM tickets WHERE id=?').get(ticket)) throw Error('工单不存在');
  const view = new Studio({ db }, null, null, dir, { readOnly: true });
  process.on('disconnect', () => process.exit(0));
  process.on('message', async (message) => {
    if (message.close) {
      process.send({ closed: true });
      return;
    }
    try {
      const { method, params } = message;
      let value;
      if (method === 'history') value = [];
      else if (method === 'defaults') value = {};
      else if (params?.ticket === ticket && ['snapshot', 'activityPage', 'jobsPage'].includes(method))
        value = await view[method](params);
      else throw Error('此窗口仅可查看所选工单的进展');
      if (process.connected) process.send({ id: message.id, value });
    } catch (e) {
      if (process.connected) process.send({ id: message.id, error: e.message });
    }
  });
  process.send({ ready: true });
} catch (e) {
  process.send({ fatal: e.message }, () => process.exit(1));
}
