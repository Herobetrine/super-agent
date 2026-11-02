'use strict';

// Only constants from these tables reach the UI. Tool arguments, results,
// private/custom tool names, paths and model-generated text stay in the ledger.
const codexTools = new Map(
  Object.entries({
    commandExecution: '命令',
    fileChange: '文件修改',
    mcpToolCall: 'MCP 工具',
    dynamicToolCall: '工具',
    collabAgentToolCall: '协作工具',
    webSearch: '网络搜索',
    imageView: '查看图片',
    imageGeneration: '生成图片'
  })
);
const deepseekTools = new Map(
  Object.entries({
    pwsh: 'PowerShell 命令',
    bash: 'Bash 命令',
    read: '读取文件',
    write: '写入文件',
    edit: '编辑文件',
    read_image: '读取图片',
    glob: '查找文件',
    grep: '搜索内容'
  })
);
const agyTools = new Map(
  Object.entries({
    read_file: '读取文件',
    write_file: '写入文件',
    command: '命令',
    read_url: '读取网页',
    mcp: 'MCP 工具'
  })
);
const states = {
  running: '执行中',
  completed: '已完成',
  failed: '失败',
  stopped: '已停止',
  unknown: '状态待确认'
};
const codexEnd = new Map([
  ['completed', 'completed'],
  ['failed', 'failed'],
  ['interrupted', 'stopped'],
  ['declined', 'stopped']
]);
const agyEnd = new Map([
  ['SUCCESS', 'completed'],
  ['ERROR', 'failed'],
  ['FAILED', 'failed']
]);
const run = (name, state) => ({ label: name + ' 轮次' + states[state], state, category: 'run' });
const tool = (name, state) => ({ label: name + ' · ' + states[state], state, category: 'tool', tool: name });

function codex(e) {
  const { turn, item } = e.params || {};
  if (e.method === 'turn/started' && turn?.id) return run('Codex', 'running');
  if (e.method === 'turn/completed' && turn?.id) {
    return run('Codex', codexEnd.get(turn.status) || 'unknown');
  }
  const name = codexTools.get(item?.type);
  if (!name) return null;
  if (e.method === 'item/started') return tool(name, 'running');
  if (e.method !== 'item/completed') return null;
  if (
    item.status === 'failed' ||
    item.success === false ||
    item.error != null ||
    item.failure != null ||
    item.result?.isError === true ||
    (typeof item.exitCode === 'number' && item.exitCode !== 0)
  ) {
    return tool(name, 'failed');
  }
  // Search and image-view items have no native success field.
  return tool(name, codexEnd.get(item.status) || 'unknown');
}

function deepseek(e) {
  if (e.type === 'status') {
    if (e.phase === 'turn_start') return run('DeepSeek', 'running');
    if (e.phase !== 'turn_end') return null;
    switch (e.reason?.kind) {
      case 'completed':
        return run('DeepSeek', 'completed');
      case 'error':
        return run('DeepSeek', 'failed');
      case 'blocked':
      case 'max-tokens':
      case 'aborted':
      case 'interrupted':
        return run('DeepSeek', 'stopped');
      default:
        return run('DeepSeek', 'unknown');
    }
  }
  if (typeof e.callId !== 'string' || !e.callId) return null;
  if (e.type === 'tool_call') return tool(deepseekTools.get(e.tool) || '工具', 'running');
  if (e.type !== 'tool_result') return null;
  // The pinned headless protocol omits the tool name on the result.
  if (e.status === 'completed') return tool('工具', 'completed');
  if (e.status === 'error') return tool('工具', 'failed');
  return tool('工具', 'unknown');
}

function antigravity(e) {
  if (e.event === 'init' && e.conversation_id) return run('Antigravity', 'running');
  if (e.event === 'result' && typeof e.result?.status === 'string') {
    return run('Antigravity', agyEnd.get(e.result.status) || 'unknown');
  }
  const s = e.step_update;
  if (e.event !== 'step_update' || s?.step_type !== 'tool') return null;
  const name = agyTools.get(s.tool_name) || '工具';
  switch (s.state) {
    case 'ACTIVE':
      return tool(name, 'running');
    case 'DONE':
      return tool(name, 'completed');
    case 'ERROR':
    case 'FAILED':
      return tool(name, 'failed');
    default:
      return null;
  }
}

function harnessActivity(event) {
  if (!event || typeof event !== 'object' || Array.isArray(event)) return null;
  if (typeof event.method === 'string') return codex(event);
  if (typeof event.event === 'string') return antigravity(event);
  return deepseek(event);
}

module.exports = { harnessActivity };
