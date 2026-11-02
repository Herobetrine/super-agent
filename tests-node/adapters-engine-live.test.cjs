'use strict';
// Explicit integration test: creates one isolated, billable real four-harness ticket.
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  path = require('node:path'),
  crypto = require('node:crypto'),
  os = require('node:os'),
  { setTimeout: sleep } = require('node:timers/promises');
const { Engine } = require('../runtime/engine.cjs');
const real = require('../runtime/adapters/index.cjs');
const hash = (b) => crypto.createHash('sha256').update(b).digest('hex');
test(
  'live Engine routes three real native terminal messages to DeepSeek, verifies the delivery file',
  { skip: process.env.SUPERAGENT_NODE_ENGINE_LIVE !== '1', timeout: 300000 },
  async () => {
    const parent = path.join(os.homedir(), '.local/share/superagent/checks');
    fs.mkdirSync(parent, { recursive: true });
    const base = fs.mkdtempSync(path.join(parent, 'node-engine-native-')),
      cwd = path.join(base, 'project');
    fs.mkdirSync(cwd);
    console.log('engine check directory', base);
    const agents = ['codex', 'claude', 'antigravity'],
      values = {},
      commands = [];
    for (const agent of agents) {
      values[agent] = crypto.randomBytes(10).toString('hex');
      fs.writeFileSync(path.join(cwd, agent + '.txt'), values[agent]);
      const body =
        'Read ' +
        agent +
        '.txt from the assigned local project. ' +
        (agent === 'codex' ? 'Write its identical bytes without adding a newline into handoff.txt. ' : '') +
        'This is an exact-output file challenge. The entire final reply must consist only of the 20-character text from that file. ' +
        'Do not emit SA_ROLE or any other SA_ control messages, preamble or markdown. Do not delegate.';
      commands.push('SA_SEND ' + JSON.stringify({ id: agent, to: agent, body, write: agent === 'codex' }));
    }
    const goal =
      'This is the primary agent instruction only. Every child must follow only its own assignment. ' +
      'On turn one output these exact three dispatch commands, one complete physical line each, then end the turn.\n' +
      commands.join('\n') +
      '\nAfter you receive all three native reports, return the three exact reported tokens grouped by sender. ' +
      'Do not send more work. Do not inspect the challenge files yourself.';
    const receipt = { children: {}, frameCounts: {}, passed: false };
    let ticket;
    const engine = new Engine(path.join(base, 'ledger'), {
      ...real,
      run: async (agent, o) => {
        const original = o.emit.batch;
        assert.equal(typeof original, 'function', 'Engine must expose its per-frame transaction');
        const stats = (receipt.frameCounts[agent] ??= { native: 0, rows: 0, grouped: 0 });
        o.emit.batch = (events) => {
          assert.equal(events.filter(([k]) => k === 'native').length, 1, 'exactly one wire frame');
          assert.equal(events[0][0], 'native');
          stats.native++;
          stats.rows += events.length;
          if (events.length > 1) stats.grouped++;
          return original(events);
        };
        return real.run(agent, o);
      }
    });
    try {
      const selections = process.env.SUPERAGENT_TEST_CODEX_SELECTION
        ? { codex: JSON.parse(process.env.SUPERAGENT_TEST_CODEX_SELECTION) }
        : undefined;
      receipt.testSelections = selections || {};
      ticket = await engine.create({
        goal,
        cwd,
        lead: 'deepseek',
        permission_profile: 'partial',
        allow_lead_routing: false,
        selections
      });
      engine.start();
      const deadline = Date.now() + 255000;
      let jobs, root;
      do {
        await sleep(250);
        if (engine.fault) throw Error(engine.fault);
        jobs = engine.all('SELECT * FROM jobs WHERE ticket=? ORDER BY id', ticket);
        root = jobs[0];
        if (['done', 'failed', 'partial', 'cancelled', 'interrupted'].includes(root.state)) break;
      } while (Date.now() < deadline);
      receipt.state = root.state;
      receipt.jobs = jobs.length;
      const events = engine.all('SELECT * FROM events WHERE ticket=? ORDER BY seq', ticket);
      const input = events.filter((e) => e.kind === 'input' && e.job === root.id).at(-1);
      const reports = input ? JSON.parse(input.payload).reports : [];
      for (const agent of agents) {
        const child = jobs.find((j) => j.parent === root.id && j.agent === agent);
        let native;
        for (const row of events.filter((e) => e.kind === 'native' && e.job === child?.id)) {
          const ev = JSON.parse(row.payload);
          if (
            agent === 'codex' &&
            ev.method === 'item/completed' &&
            ev.params.item.type === 'agentMessage' &&
            ev.params.item.phase !== 'commentary'
          )
            native = ev.params.item.text;
          if (agent === 'claude' && ev.type === 'result') native = ev.result;
          if (agent === 'antigravity' && ev.event === 'result') native = ev.result.response;
        }
        const delivered = reports.find((r) => r.id === child?.id)?.result;
        receipt.children[agent] = {
          state: child?.state,
          challenge: child?.result?.trim() === values[agent],
          exactDelivery: typeof native === 'string' && native === child.result && native === delivered,
          rootIncludes: root.result?.includes(values[agent])
        };
      }
      const output = path.join(cwd, 'handoff.txt');
      receipt.exactFile = fs.existsSync(output) && fs.readFileSync(output).equals(Buffer.from(values.codex));
      receipt.events = events.length;
      receipt.exactHashes = events.every((e) => hash(e.payload) === e.sha256);
      receipt.passed =
        jobs.length === 4 &&
        root.state === 'done' &&
        receipt.exactFile &&
        receipt.exactHashes &&
        Object.values(receipt.frameCounts).length === 4 &&
        Object.values(receipt.frameCounts).every((c) => c.grouped > 0) &&
        Object.values(receipt.children).every(
          (r) => r.state === 'done' && r.challenge && r.exactDelivery && r.rootIncludes
        );
      assert.ok(receipt.passed, 'four-harness receipt mismatch: ' + JSON.stringify(receipt));
    } finally {
      if (ticket && !receipt.passed) engine.cancel({ ticket });
      await engine.close();
      fs.writeFileSync(path.join(base, 'receipt.json'), JSON.stringify(receipt, null, 2));
    }
  }
);
