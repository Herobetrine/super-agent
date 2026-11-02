'use strict';

const { harnessActivity } = require('./harness-activity.cjs');

const tools = new Set([
  'Agent',
  'Task',
  'Bash',
  'PowerShell',
  'Read',
  'Write',
  'Edit',
  'MultiEdit',
  'Glob',
  'Grep',
  'NotebookEdit',
  'WebFetch',
  'WebSearch',
  'TaskCreate',
  'TaskUpdate',
  'TaskList',
  'TaskGet',
  'TaskOutput',
  'TaskStop',
  'TodoWrite',
  'Skill',
  'AskUserQuestion',
  'EnterPlanMode',
  'ExitPlanMode'
]);
const roles = new Set(['Explore', 'Plan', 'general-purpose', 'code-reviewer', 'claude-code-guide']);
const statusLabels = {
  running: '执行中',
  completed: '已完成',
  failed: '失败',
  stopped: '已停止',
  unknown: '状态待确认'
};
const terminalStates = new Set(['completed', 'failed', 'stopped']);
const privateState = new WeakMap();

function nativeId(value) {
  return typeof value === 'string' && /^[A-Za-z0-9_-]{1,160}$/.test(value) ? value : null;
}

function toolLabel(name) {
  if (tools.has(name)) return name;
  return typeof name === 'string' && name.startsWith('mcp__') ? 'MCP 工具' : '工具';
}

function taskLabel(e) {
  if (roles.has(e.subagent_type)) return `子代理 · ${e.subagent_type}`;
  if (e.task_type === 'local_agent' || e.task_type === 'remote_agent' || e.subagent_type) return '子代理';
  if (e.task_type === 'local_bash') return '后台命令';
  return '原生任务';
}
// Only Claude's short, operator-facing Agent title. Tool arguments, the
// assignment prompt, progress payloads and terminal summaries are not titles.
function taskTitle(e) {
  if (e.subtype !== 'task_started' || e.task_type !== 'local_agent' || typeof e.description !== 'string')
    return null;
  const s = e.description.trim();
  if (s.length < 2 || s.length > 140 || !/[\p{Script=Han}\s]/u.test(s)) return null;
  if (!/^[\p{L}\p{N} ()（）、，。:：&+/-]+$/u.test(s) || /(?:^|\s)\/|\/\/|[\w-]{32,}/u.test(s)) return null;
  return s;
}

function status(value) {
  if (value === 'killed' || value === 'cancelled' || value === 'canceled') return 'stopped';
  if (typeof value === 'string' && Object.hasOwn(statusLabels, value)) return value;
  return 'unknown';
}

function taskActivity(e) {
  const taskId = nativeId(e.task_id);
  if (!taskId || e.ambient === true) return null;
  let state;
  switch (e.subtype) {
    case 'task_started':
    case 'task_progress':
      state = 'running';
      break;
    case 'task_notification':
      state = status(e.status);
      break;
    case 'task_updated':
      if (typeof e.patch?.status !== 'string') return null;
      state = status(e.patch.status);
      break;
    default:
      return null;
  }
  const tool = typeof e.last_tool_name === 'string' && e.last_tool_name ? toolLabel(e.last_tool_name) : null;
  const detail = e.subtype === 'task_started' ? '已启动' : statusLabels[state];
  return {
    label: taskLabel(e) + detail + (tool ? ` · ${tool}` : ''),
    state,
    category: 'task',
    taskId,
    ...(tool && { tool })
  };
}

function toolBlocks(e) {
  if (e.type !== 'assistant' && e.type !== 'user') return [];
  const content = e.message?.content;
  const type = e.type === 'assistant' ? 'tool_use' : 'tool_result';
  return Array.isArray(content) ? content.filter((part) => part?.type === type) : [];
}

function toolActivity(e, blocks, lookup) {
  const started = e.type === 'assistant';
  const names = [
    ...new Set(blocks.map((b) => (started ? toolLabel(b.name) : lookup?.(b.tool_use_id) || '工具')))
  ];
  const tool = names.join('、');
  const count = blocks.length > 1 ? `（${blocks.length} 项）` : '';
  const failed = !started && blocks.some((b) => b.is_error === true);
  const label = started ? `调用 ${tool}` : `${tool}${failed ? '返回错误' : '已返回'}`;
  const item = {
    label: label + count,
    category: 'tool',
    state: started ? 'running' : failed ? 'failed' : 'completed',
    tool
  };
  if (blocks.length > 1)
    item.steps = blocks.map((b) => ({
      tool: started ? toolLabel(b.name) : lookup?.(b.tool_use_id) || '工具',
      state: started ? 'running' : b.is_error === true ? 'failed' : 'completed'
    }));
  return item;
}

function eventFor(row) {
  if (row?.kind && row.kind !== 'native') return null;
  try {
    const e = typeof row?.payload === 'string' ? JSON.parse(row.payload) : row?.payload;
    return e && typeof e === 'object' && !Array.isArray(e) ? e : null;
  } catch {
    return null;
  }
}

function classify(e, blocks, lookup) {
  if (e.type === 'system') {
    if (e.subtype === 'init') return { label: 'Claude Code 已启动', state: 'running', category: 'run' };
    if (e.subtype === 'api_retry') return { label: '等待模型请求重试', state: 'running', category: 'run' };
    if (e.subtype === 'status' && e.status === 'compacting')
      return { label: '正在整理会话上下文', state: 'running', category: 'run' };
    const task = taskActivity(e);
    if (task) return task;
  }
  if (blocks.length) return toolActivity(e, blocks, lookup);
  if (e.type === 'tool_progress' && !e.heartbeat && typeof e.tool_name === 'string') {
    const tool = toolLabel(e.tool_name);
    const label = e.subagent_retry ? '子代理等待模型请求重试' : `${tool} 执行中`;
    return { label, state: 'running', category: 'tool', tool };
  }
  if (e.type === 'result' && (typeof e.is_error === 'boolean' || e.subtype === 'success')) {
    const ok = !e.is_error && e.subtype === 'success';
    return {
      label: ok ? '原生轮次已完成' : '原生轮次失败',
      state: ok ? 'completed' : 'failed',
      category: 'run'
    };
  }
  return harnessActivity(e);
}

function activityFor(row) {
  const e = eventFor(row);
  return e && classify(e, toolBlocks(e));
}

function createActivity() {
  const state = { tasks: [], latest: null };
  privateState.set(state, {
    tasks: new Map(),
    parents: new Map(),
    calls: new Map(),
    backgrounds: new Map(),
    ambient: new Set(),
    seq: 0
  });
  return state;
}

// Consume journal rows in increasing sequence order. Only correlation keys and display labels survive.
function applyActivity(state, row) {
  const index = privateState.get(state);
  if (row.seq <= index.seq) return null;
  index.seq = row.seq;
  const e = eventFor(row);
  if (!e) return null;
  const { job, turn, ts } = row;
  const key = (id) => JSON.stringify([job, turn, id]);
  const updateAmbient = (id, ambient) => {
    const scoped = key(id);
    if (ambient === false) index.ambient.delete(scoped);
    if (ambient !== true) return;
    index.ambient.add(scoped);
    const rec = index.tasks.get(scoped);
    if (rec) {
      state.tasks.splice(state.tasks.indexOf(rec.task), 1);
      index.tasks.delete(scoped);
      for (const [parent, link] of index.parents) {
        if (link?.rec === rec) index.parents.delete(parent);
      }
    }
  };
  const upsert = (native, id, nextState) => {
    const scoped = key(id);
    if (index.ambient.has(scoped)) return null;
    let rec = index.tasks.get(scoped);
    const run = nativeId(native.run_id);
    // The SDK guarantees lexical start order for the run IDs of a single resumed task.
    if (run && rec?.run && run < rec.run) return null;
    const resumed = run && rec?.run && run > rec.run;
    if (rec && !resumed && terminalStates.has(rec.task.state) && !terminalStates.has(nextState)) return null;
    if (!rec) {
      const task = { id, job, turn, label: taskLabel(native), state: nextState, lastTool: '', ts };
      rec = { task, run };
      index.tasks.set(scoped, rec);
      state.tasks.push(task);
    } else if (resumed) {
      rec.task.lastTool = '';
      delete rec.task.toolUses;
    }
    if (run) rec.run = run;
    const { task } = rec;
    const title = taskTitle(native);
    if (title) task.title = title;
    task.state = nextState;
    task.ts = ts;
    if (roles.has(native.subagent_type) || task.label === '原生任务') task.label = taskLabel(native);
    const parent = nativeId(native.tool_use_id);
    if (parent) {
      const alias = key(parent);
      const previous = index.parents.get(alias);
      // An ambiguous tool-use link cannot establish ownership of later nested calls.
      index.parents.set(
        alias,
        previous === undefined || previous?.rec === rec ? { rec, run: rec.run } : null
      );
    }
    return task;
  };
  if (e.type === 'system' && e.subtype === 'background_tasks_changed' && Array.isArray(e.tasks)) {
    const scope = key(null);
    const live = new Set();
    for (const entry of e.tasks) {
      const id = nativeId(entry?.task_id);
      if (!id) continue;
      updateAmbient(id, entry.ambient);
      const scoped = key(id);
      live.add(scoped);
      const rec = index.tasks.get(scoped);
      if (!rec || rec.task.state === 'unknown' || (entry.run_id && entry.run_id !== rec.run))
        upsert(entry, id, 'running');
    }
    // Absence from a new full snapshot ends observed liveness, without supplying a terminal outcome.
    for (const scoped of index.backgrounds.get(scope) || []) {
      const task = index.tasks.get(scoped)?.task;
      if (!live.has(scoped) && task?.state === 'running') {
        task.state = 'unknown';
        task.ts = ts;
      }
    }
    index.backgrounds.set(scope, live);
    return null;
  }
  if (e.type === 'system' && nativeId(e.task_id)) updateAmbient(e.task_id, e.ambient);
  const blocks = toolBlocks(e);
  const activity = classify(e, blocks, (id) => index.calls.get(key(nativeId(id))));
  if (!activity) return null;
  if (activity.category === 'task' && activity.taskId) {
    const task = upsert(e, activity.taskId, activity.state);
    if (!task) return null;
    const uses = e.usage?.tool_uses;
    if (Number.isSafeInteger(uses) && uses >= 0) task.toolUses = Math.max(task.toolUses || 0, uses);
    if (activity.tool) task.lastTool = activity.tool;
    if (task.title) activity.title = task.title;
    activity.label = activity.label.replace(taskLabel(e), task.label);
  } else if (blocks.length || e.type === 'tool_progress') {
    const link = index.parents.get(key(nativeId(e.parent_tool_use_id)));
    const owner = link && link.run === link.rec.run ? link.rec.task : null;
    for (const block of blocks) {
      const id = nativeId(block.id || block.tool_use_id);
      if (!id) continue;
      if (block.type === 'tool_use') index.calls.set(key(id), toolLabel(block.name));
      else index.calls.delete(key(id));
    }
    if (owner) {
      activity.taskId = owner.id;
      if (!terminalStates.has(owner.state)) {
        owner.lastTool = activity.tool;
        owner.ts = ts;
      }
    }
  }
  state.latest = { job, turn, label: activity.label, ts };
  return activity;
}

function projectActivity(rows) {
  const state = createActivity();
  for (const row of rows) applyActivity(state, row);
  return state;
}

module.exports = { activityFor, createActivity, applyActivity, projectActivity };
