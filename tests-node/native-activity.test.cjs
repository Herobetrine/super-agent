'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  activityFor,
  createActivity,
  applyActivity,
  projectActivity
} = require('../runtime/native-activity.cjs');

function row(seq, payload, job = 15, turn = 1) {
  return { kind: 'native', job, turn, seq, ts: 1000 + seq, payload: JSON.stringify(payload) };
}

const task = (subtype, fields) => ({ type: 'system', subtype, ...fields });
const started = task('task_started', {
  task_id: 'a-one',
  tool_use_id: 'tool-start',
  task_type: 'local_agent',
  subagent_type: 'Explore',
  description: 'PRIVATE',
  prompt: 'PRIVATE'
});
const tool = (type, content, parent_tool_use_id) => ({ type, message: { content }, parent_tool_use_id });
const command = { type: 'tool_use', name: 'Bash', id: 'cmd-1', input: { command: 'PRIVATE' } };
const result = { type: 'tool_result', tool_use_id: 'cmd-1', content: 'PRIVATE' };

test('Claude task cards use a short assignment title without exposing the prompt or shell input', () => {
  const state = createActivity();
  const event = applyActivity(
    state,
    row(1, {
      ...started,
      subtype: 'task_started',
      task_type: 'local_agent',
      description: 'Audit gameplay loop'
    })
  );
  assert.equal(event.title, 'Audit gameplay loop');
  assert.equal(state.tasks[0].title, 'Audit gameplay loop');
  applyActivity(
    state,
    row(2, {
      ...started,
      task_id: 'second',
      task_type: 'local_agent',
      description: 'read C:\\private\\file.json'
    })
  );
  assert.equal(state.tasks[1].title, undefined);
  assert.ok(!JSON.stringify(state).includes('PRIVATE'));
});

test('Claude background Agent results do not complete tasks; lifecycle and nested calls share native identity', () => {
  const state = createActivity();
  const apply = (seq, event) => applyActivity(state, row(seq, event));
  apply(1, tool('assistant', [{ type: 'tool_use', name: 'Agent', id: 'tool-start' }]));
  assert.equal(state.tasks.length, 0);
  apply(2, task('background_tasks_changed', { tasks: [started] }));
  apply(3, started);
  apply(4, tool('user', [{ type: 'tool_result', tool_use_id: 'tool-start' }]));
  assert.equal(state.tasks[0].state, 'running');
  assert.equal(apply(5, tool('assistant', [command], 'tool-start')).taskId, 'a-one');
  const reply = apply(6, tool('user', [result], 'tool-start'));
  assert.equal(reply.taskId, 'a-one');
  assert.equal(reply.tool, 'Bash');
  apply(7, task('task_progress', { task_id: 'a-one', last_tool_name: 'Read', usage: { tool_uses: 2 } }));
  apply(8, task('task_updated', { task_id: 'a-one', patch: { status: 'completed' } }));
  apply(9, task('task_notification', { task_id: 'a-one', status: 'completed', summary: 'PRIVATE' }));
  assert.equal(apply(10, task('task_progress', { task_id: 'a-one', last_tool_name: 'Bash' })), null);
  assert.deepEqual(state.tasks, [
    {
      id: 'a-one',
      job: 15,
      turn: 1,
      state: 'completed',
      ts: 1009,
      label: '子代理 · Explore',
      lastTool: 'Read',
      toolUses: 2
    }
  ]);
  assert.equal(state.latest.ts, 1009);
  assert.doesNotMatch(JSON.stringify([state, reply]), /PRIVATE|tool-start|cmd-1/);
});

test('same task identity is isolated by job and turn and absent snapshot entries do not infer a terminal state', () => {
  const state = projectActivity([
    row(1, started),
    row(2, started, 16),
    row(3, started, 15, 2),
    row(4, task('background_tasks_changed', { tasks: [] })),
    row(5, task('task_notification', { task_id: 'a-one', status: 'failed' }), 16),
    row(6, task('task_updated', { task_id: 'a-one', patch: { status: 'stopped' } }), 15, 2)
  ]);
  assert.deepEqual(
    state.tasks.map((t) => t.state),
    ['running', 'failed', 'stopped']
  );
});

test('only whitelisted tool and role names project; tool blocks can be batched and errors remain visible', () => {
  const state = createActivity();
  assert.equal(
    activityFor(row(1, task('task_started', { ...started, subagent_type: 'PRIVATE' }))).label,
    '子代理已启动'
  );
  const mcp = { ...command, name: 'mcp__PRIVATE__tool', id: 'cmd-2' };
  const activity = applyActivity(state, row(1, tool('assistant', [command, mcp])));
  assert.match(activity.label, /Bash、MCP 工具.*2 项/);
  const outcome = applyActivity(
    state,
    row(2, tool('user', [result, { ...result, tool_use_id: 'cmd-2', is_error: true }]))
  );
  assert.equal(outcome.state, 'failed');
  assert.equal(state.tasks.length, 0);
  assert.doesNotMatch(JSON.stringify([state, activity, outcome]), /PRIVATE/);
});

test('malformed rows, reasoning, argument deltas, plain chat and progress without identity are omitted', () => {
  const cases = [
    { kind: 'native', payload: '{' },
    { kind: 'native', payload: 'null' },
    { kind: 'delta', payload: 'PRIVATE' },
    row(
      1,
      tool('assistant', [
        { type: 'thinking', thinking: 'PRIVATE' },
        { type: 'text', text: 'PRIVATE' }
      ])
    ),
    row(2, { type: 'stream_event', event: { delta: { type: 'input_json_delta', partial_json: 'PRIVATE' } } }),
    row(3, task('task_progress', { description: 'PRIVATE' })),
    row(4, task('thinking_tokens', { tokens: 12 })),
    row(5, { type: 'tool_progress', tool_name: 'Bash', heartbeat: true })
  ];
  for (const r of cases) assert.equal(activityFor(r), null);
  assert.deepEqual(projectActivity(cases), { tasks: [], latest: null });
  assert.equal(
    activityFor(row(5, task('task_notification', { task_id: 'a', status: 'unexpected' }))).state,
    'unknown'
  );
  assert.equal(
    activityFor(row(6, { type: 'result', subtype: 'success', result: 'PRIVATE' })).state,
    'completed'
  );
  assert.equal(
    activityFor(row(7, { type: 'result', subtype: 'error_during_execution', is_error: true })).state,
    'failed'
  );
});

test('API backoff, context compaction and non-heartbeat tool progress produce safe activity', () => {
  const state = createActivity();
  applyActivity(state, row(1, started));
  const event = {
    type: 'tool_progress',
    tool_name: 'Bash',
    parent_tool_use_id: 'tool-start',
    subagent_retry: { error: 'PRIVATE' }
  };
  const latest = applyActivity(state, row(2, event));
  assert.equal(latest.taskId, 'a-one');
  assert.equal(state.tasks[0].lastTool, 'Bash');
  assert.match(latest.label, /重试/);
  assert.doesNotMatch(JSON.stringify(latest), /PRIVATE/);
  assert.match(activityFor(row(3, task('status', { status: 'compacting' }))).label, /上下文/);
  assert.match(activityFor(row(4, task('api_retry', {}))).label, /重试/);
});

test('resuming a native task ignores late events from its earlier run and falls back to task ID for legacy notifications', () => {
  const state = createActivity();
  const apply = (seq, event) => applyActivity(state, row(seq, event));
  apply(1, { ...started, run_id: '1' });
  apply(
    2,
    task('task_progress', { task_id: 'a-one', run_id: '1', last_tool_name: 'Bash', usage: { tool_uses: 8 } })
  );
  apply(3, task('task_notification', { task_id: 'a-one', run_id: '1', status: 'completed' }));
  apply(4, { ...started, run_id: '2', tool_use_id: 'new-start' });
  assert.equal(
    apply(5, task('task_notification', { task_id: 'a-one', run_id: '1', status: 'failed' })),
    null
  );
  assert.equal(state.tasks.length, 1);
  assert.equal(state.tasks[0].state, 'running');
  assert.equal(state.tasks[0].lastTool, '');
  assert.equal(state.tasks[0].toolUses, undefined);
  apply(6, task('task_notification', { task_id: 'a-one', status: 'completed' }));
  assert.equal(state.tasks[0].state, 'completed');
});

test('ambient tasks stay hidden; unreported background removals lose running status without inferring success', () => {
  const snap = (...tasks) => task('background_tasks_changed', { tasks });
  const state = projectActivity([
    row(1, snap(started, { task_id: 'watcher', ambient: true })),
    row(2, started),
    row(3, task('task_progress', { task_id: 'watcher', usage: { tool_uses: 8 } })),
    row(4, snap())
  ]);
  assert.equal(state.tasks.length, 1);
  assert.equal(state.tasks[0].state, 'unknown');
  applyActivity(state, row(5, snap({ ...started, ambient: true })));
  assert.equal(state.tasks.length, 0);
  applyActivity(state, row(6, snap({ ...started, ambient: false })));
  assert.equal(state.tasks[0].state, 'running');
});

test('a tool parent ID shared by two different tasks does not misattribute nested work', () => {
  const state = createActivity();
  applyActivity(state, row(1, started));
  applyActivity(state, row(2, { ...started, task_id: 'a-two' }));
  const event = applyActivity(state, row(3, tool('assistant', [command], 'tool-start')));
  assert.equal(event.taskId, undefined);
  assert.deepEqual(
    state.tasks.map((t) => t.lastTool),
    ['', '']
  );
});
