'use strict';
const test = require('node:test'),
  assert = require('node:assert/strict');
const common = require('../runtime/adapters/common.cjs'),
  config = require('../runtime/adapters/codex-config.cjs');
const peer = async function peer() {
  const lines = require('node:readline')
    .createInterface({ input: process.stdin, crlfDelay: Infinity })
    [Symbol.asyncIterator]();
  const recv = async () => JSON.parse((await lines.next()).value),
    send = (o) => process.stdout.write(JSON.stringify(o) + '\n');
  await recv();
  send({ id: 1, result: {} });
  await recv();
  const thread = await recv();
  if (thread.params.sandbox !== 'workspace-write' || thread.params.approvalPolicy !== 'on-request')
    process.exit(3);
  send({
    id: 2,
    result: {
      sandbox: { type: 'workspaceWrite', networkAccess: false, writableRoots: [] },
      model: 'm',
      modelProvider: 'p',
      approvalPolicy: 'on-request',
      approvalsReviewer: 'user',
      thread: { id: 'thread' }
    }
  });
  await recv();
  send({ id: 3, result: { turn: { id: 'turn' } } });
  const steer = await recv();
  if (
    steer.method !== 'turn/steer' ||
    steer.params.expectedTurnId !== 'turn' ||
    !steer.params.input[0].text.includes('消息')
  )
    process.exit(4);
  send({ id: steer.id, result: { turnId: 'turn' } });
  const params = { threadId: 'thread', turnId: 'turn', itemId: 'item', command: 'review this command' };
  send({ method: 'item/commandExecution/requestApproval', id: 24, params });
  if ((await recv()).result.decision !== 'accept') process.exit(5);
  send({ method: 'item/commandExecution/requestApproval', id: 25, params: { ...params, turnId: 'old' } });
  if ((await recv()).result.decision !== 'decline') process.exit(6);
  send({ method: 'item/agentMessage/delta', params: { ...params, delta: '正文\t😀\r\n' } });
  send({
    method: 'item/completed',
    params: { ...params, item: { id: 'item', type: 'agentMessage', text: '正文\t😀\r\n' } }
  });
  send({
    method: 'turn/completed',
    params: { threadId: 'thread', turn: { id: 'turn', status: 'completed' } }
  });
};
test('real stdio framing handles Codex approval, stale request denial, native steer acknowledgement and exact final', async (t) => {
  const realStart = common.start;
  t.mock.method(common, 'capture', async () => '[]');
  t.mock.method(common, 'start', (_exe, args, opts) => {
    assert.ok(args.includes('sandbox_workspace_write.network_access=false'));
    assert.ok(args.includes('--disable'));
    return realStart(process.execPath, ['-e', '(' + peer.toString() + ')()'], opts);
  });
  t.mock.method(config, 'discover', async () => ({
    exe: 'fake',
    catalog: {
      default: { provider: 'p', model: 'm', mode: 'cli' },
      modes: [{ id: 'cli', available: true }],
      providers: [{ id: 'p', models: [{ id: 'm' }] }]
    }
  }));
  t.mock.method(config, 'routing', () => ({ args: [], settings: {} }));
  const id = require.resolve('../runtime/adapters/codex.cjs');
  delete require.cache[id];
  t.after(() => {
    delete require.cache[id];
  });
  const { run } = require(id),
    events = [],
    actions = [],
    accepted = [];
  const result = await run({
    cwd: process.cwd(),
    profile: 'partial',
    writable: true,
    prompt: 'test',
    signal: AbortSignal.timeout(5000),
    emit: (k, v) => events.push([k, v]),
    approve: async (action) => {
      actions.push(action);
      return true;
    },
    pendingNotes: () => (accepted.length ? [] : [{ seq: 7, sender: 'claude', body: '消息' }]),
    acceptNotes: (s) => accepted.push(...s)
  });
  assert.deepEqual(result, { text: '正文\t😀\r\n', sessionId: 'thread' });
  assert.deepEqual(accepted, [7]);
  assert.equal(actions.length, 1);
  assert.equal(actions[0].params.command, 'review this command');
  assert.deepEqual(
    events.filter(([k]) => ['delta', 'message.boundary'].includes(k)),
    [
      ['delta', result.text],
      ['message.boundary', null]
    ]
  );
});
