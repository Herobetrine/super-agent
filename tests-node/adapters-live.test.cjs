'use strict';
// Explicit, billable acceptance check. Normal unit tests never start a model.
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  path = require('node:path'),
  os = require('node:os'),
  crypto = require('node:crypto');
const { catalog, run } = require('../runtime/adapters/index.cjs');
const hash = (s) => crypto.createHash('sha256').update(s, 'utf8').digest('hex');
test(
  'four real read-only harnesses return file challenges and exact terminal text',
  { skip: process.env.SUPERAGENT_NODE_LIVE !== '1', timeout: 330000 },
  async () => {
    const parent = path.join(os.homedir(), '.local/share/superagent/checks');
    fs.mkdirSync(parent, { recursive: true });
    const base = fs.mkdtempSync(path.join(parent, 'node-native-')),
      receipts = {};
    async function check(agent) {
      const project = path.join(base, agent);
      fs.mkdirSync(project);
      const nonce = crypto.randomBytes(12).toString('hex');
      fs.writeFileSync(path.join(project, 'challenge.txt'), nonce);
      const row = (receipts[agent] = { passed: false, events: 0, expected: hash(nonce) });
      try {
        const c = await catalog(agent);
        row.selection = c.default;
        let native;
        const result = await run(agent, {
          cwd: project,
          profile: 'readonly',
          writable: false,
          selection: c.default,
          prompt:
            'Read challenge.txt in this project. Return only the exact text inside it. Do not modify any files.',
          signal: AbortSignal.timeout(135000),
          emit: (kind, e) => {
            if (kind !== 'native') return;
            row.events++;
            if (
              agent === 'codex' &&
              e.method === 'item/completed' &&
              e.params.item.type === 'agentMessage' &&
              e.params.item.phase !== 'commentary'
            )
              native = e.params.item.text;
            if (agent === 'claude' && e.type === 'result') native = e.result;
            if (agent === 'deepseek' && e.type === 'final') native = e.text;
            if (agent === 'antigravity' && e.event === 'result') native = e.result.response;
          }
        });
        row.answer = hash(result.text);
        row.native = typeof native === 'string' ? hash(native) : null;
        row.challengeMatched = result.text.trim() === nonce;
        row.sessionReturned = !!result.sessionId;
        row.passed = row.challengeMatched && result.text === native && row.sessionReturned;
      } catch (e) {
        row.error = e.message;
      }
    }
    for (const group of [
      ['codex', 'claude'],
      ['deepseek', 'antigravity']
    ])
      await Promise.all(group.map(check));
    const file = path.join(base, 'receipt.json');
    fs.writeFileSync(file, JSON.stringify(receipts, null, 2));
    console.log('native receipt', file);
    for (const [agent, row] of Object.entries(receipts))
      assert.equal(row.passed, true, agent + ': ' + (row.error || 'challenge or terminal mismatch'));
  }
);
