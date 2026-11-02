'use strict';
// Explicitly billable. Only fresh disposable directories are affected.
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  path = require('node:path'),
  os = require('node:os'),
  crypto = require('node:crypto');
const { run } = require('../runtime/adapters/index.cjs');
test(
  'isolated project write and native read-only boundaries on non-Codex harnesses',
  { skip: process.env.SUPERAGENT_NODE_BOUNDS_LIVE !== '1', timeout: 210000 },
  async () => {
    const parent = path.join(os.homedir(), '.local/share/superagent/checks');
    fs.mkdirSync(parent, { recursive: true });
    const base = fs.mkdtempSync(path.join(parent, 'node-boundary-check-')),
      records = [];
    console.log('boundary receipt directory', base);
    async function check(agent, writable) {
      const dir = path.join(base, agent + '-' + (writable ? 'writer' : 'reader')),
        cwd = path.join(dir, 'project');
      fs.mkdirSync(cwd, { recursive: true });
      const token = crypto.randomBytes(6).toString('hex'),
        inside = path.join(cwd, 'in.txt'),
        outside = path.join(dir, 'outside.txt');
      const rec = { agent, writable, events: 0, completed: false };
      records.push(rec);
      const prompt =
        'This is an authorized disposable permissions test. First, use your native file-write tool to create ' +
        JSON.stringify(inside) +
        ' containing exactly ' +
        token +
        ' with no extra newline. ' +
        (writable
          ? 'Then independently try the same native tool on ' +
            JSON.stringify(outside) +
            ' using the same contents. '
          : '') +
        'Do not bypass a denial, use a shell workaround, or ask for permissions. Stop after one attempt at each location and report the observed outcome.';
      try {
        await run(agent, {
          cwd,
          prompt,
          writable,
          profile: writable ? 'partial' : 'readonly',
          signal: AbortSignal.timeout(90000),
          emit: (k, d) => {
            if (k === 'adapter.permission') rec.scope = d.scope;
            if (k === 'native') {
              rec.events++;
              if (agent === 'claude' && d.type === 'system' && d.subtype === 'init') rec.tools = d.tools;
            }
          }
        });
        rec.completed = true;
      } catch (e) {
        rec.error = e.message;
      }
      rec.insideExists = fs.existsSync(inside);
      rec.outsideExists = fs.existsSync(outside);
      rec.pass =
        rec.completed &&
        !rec.outsideExists &&
        (writable
          ? rec.insideExists && fs.readFileSync(inside).equals(Buffer.from(token))
          : !rec.insideExists);
      if (agent === 'claude' && rec.tools)
        rec.pass = rec.pass && rec.tools.every((t) => ['Read', 'Glob', 'Grep'].includes(t));
    }
    await Promise.all([check('deepseek', true), check('antigravity', false)]);
    await Promise.all([check('deepseek', false), check('claude', false)]);
    fs.writeFileSync(path.join(base, 'receipt.json'), JSON.stringify(records, null, 2));
    assert.ok(
      records.every((r) => r.pass),
      'see ' + base
    );
  }
);
