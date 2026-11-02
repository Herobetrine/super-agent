'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { harnessActivity } = require('../runtime/harness-activity.cjs');

const privateText = 'PRIVATE-SENTINEL';
function check(event, state, category, tool) {
  const view = harnessActivity(event);
  assert.equal(view?.state, state);
  assert.equal(view.category, category);
  assert.equal(view.tool, tool);
  assert.ok(view.label);
  assert.ok(!JSON.stringify(view).includes(privateText));
  assert.deepEqual(
    Object.keys(view).sort(),
    tool ? ['category', 'label', 'state', 'tool'] : ['category', 'label', 'state']
  );
}

test('DeepSeek headless turn reason controls outcome even when a final text follows', () => {
  check({ type: 'status', phase: 'turn_start', turn: 1 }, 'running', 'run');
  for (const [kind, state] of [
    ['completed', 'completed'],
    ['aborted', 'stopped'],
    ['error', 'failed'],
    ['future', 'unknown']
  ]) {
    check({ type: 'status', phase: 'turn_end', reason: { kind, error: privateText } }, state, 'run');
  }
  assert.equal(harnessActivity({ type: 'final', text: privateText }), null);
});

test('DeepSeek tool calls expose only known display names and tool results carry no name', () => {
  for (const [tool, label] of [
    ['read', '读取文件'],
    ['pwsh', 'PowerShell 命令'],
    [privateText, '工具']
  ]) {
    check(
      { type: 'tool_call', callId: privateText, tool, input: { path: privateText } },
      'running',
      'tool',
      label
    );
  }
  for (const [status, state] of [
    ['error', 'failed'],
    ['completed', 'completed'],
    ['future', 'unknown']
  ]) {
    check({ type: 'tool_result', callId: privateText, status, result: privateText }, state, 'tool', '工具');
  }
  assert.equal(harnessActivity({ type: 'tool_call', tool: 'read' }), null);
});

test('Codex projects native turn and tool outcomes including process, patch and MCP failure', () => {
  const turn = (method, status) => ({
    method,
    params: { turn: { id: privateText, status, error: privateText } }
  });
  check(turn('turn/started', 'inProgress'), 'running', 'run');
  for (const [status, state] of [
    ['interrupted', 'stopped'],
    ['failed', 'failed'],
    ['completed', 'completed']
  ]) {
    check(turn('turn/completed', status), state, 'run');
  }
  const item = (method, item) => ({ method, params: { item } });
  const cmd = {
    type: 'commandExecution',
    command: privateText,
    cwd: privateText,
    aggregatedOutput: privateText
  };
  check(item('item/started', cmd), 'running', 'tool', '命令');
  for (const [exitCode, state] of [
    [1, 'failed'],
    [0, 'completed']
  ]) {
    check(item('item/completed', { ...cmd, status: 'completed', exitCode }), state, 'tool', '命令');
  }
  check(
    item('item/completed', { type: 'fileChange', status: 'declined', changes: [privateText] }),
    'stopped',
    'tool',
    '文件修改'
  );
  check(
    item('item/completed', {
      type: 'mcpToolCall',
      tool: privateText,
      status: 'completed',
      result: { isError: true }
    }),
    'failed',
    'tool',
    'MCP 工具'
  );
  check(
    item('item/completed', { type: 'dynamicToolCall', tool: privateText, status: 'failed' }),
    'failed',
    'tool',
    '工具'
  );
  check(item('item/completed', { type: 'webSearch', query: privateText }), 'unknown', 'tool', '网络搜索');
});

test('Antigravity shows tool steps and terminal results with constant labels', () => {
  check({ event: 'init', conversation_id: privateText }, 'running', 'run');
  for (const [status, state] of [
    ['SUCCESS', 'completed'],
    ['ERROR', 'failed'],
    ['future', 'unknown']
  ]) {
    check({ event: 'result', result: { status, response: privateText } }, state, 'run');
  }
  for (const [mark, state] of [
    ['ACTIVE', 'running'],
    ['DONE', 'completed'],
    ['FAILED', 'failed']
  ]) {
    const step = {
      step_type: 'tool',
      state: mark,
      tool_name: 'read_file',
      tool_info: { output: privateText }
    };
    check({ event: 'step_update', step_update: step }, state, 'tool', '读取文件');
    check({ event: 'step_update', step_update: { ...step, tool_name: privateText } }, state, 'tool', '工具');
  }
});

test('malformed frames, text, reasoning, pings and unrelated fields never become tool activities', () => {
  for (const raw of [
    null,
    [],
    3,
    {},
    'ping',
    { type: 'thinking' },
    { type: 'text', text: privateText },
    { event: 'ping' },
    { type: 'session', cwd: privateText },
    { type: 'status', phase: 'step_end' },
    { type: 'tool_result', callId: {} },
    { event: 'result' },
    { method: 'turn/completed', params: null },
    { method: 'item/completed', params: { item: { type: { toString: null } } } },
    { method: 'item/agentMessage/delta', params: { delta: privateText } },
    { method: 'item/completed', params: { item: { type: 'reasoning', summary: privateText } } },
    {
      event: 'step_update',
      step_update: { step_type: 'agent_response', state: 'DONE', text_delta: privateText }
    }
  ]) {
    assert.equal(harnessActivity(raw), null);
  }
});
