'use strict';

const { createHash } = require('node:crypto');
const AGENTS = ['codex', 'claude', 'deepseek', 'antigravity'];
const PROFILES = ['readonly', 'ask', 'partial', 'full', 'auto'];
const TERMINAL = ['done', 'partial', 'failed', 'cancelled'];
const isFull = (profile) => profile === 'full' || profile === 'auto';
const canWrite = (agent, profile) => isFull(profile) || ['codex', 'deepseek'].includes(agent);
const validText = (x, max = Infinity) => typeof x === 'string' && x.trim().length > 0 && x.length <= max;
const isObj = (x) => !!x && typeof x === 'object' && !Array.isArray(x);
const ident = (x) => validText(x, 256) && !/[\p{C}\p{Zl}\p{Zp}]/u.test(x);
const digest = (text) => createHash('sha256').update(text, 'utf8').digest('hex');
const childProfiles = (p) =>
  PROFILES.includes(p) ? PROFILES.slice(0, PROFILES.indexOf(p) + 1) : ['readonly'];

function route(x) {
  if (
    !isObj(x) ||
    Object.keys(x).length !== 3 ||
    !ident(x.provider) ||
    !ident(x.model) ||
    !['cli', 'desktop'].includes(x.mode)
  )
    throw new Error('无效的模型、供应商或执行端');
  return { provider: x.provider, model: x.model, mode: x.mode };
}

function routes(obj) {
  if (!isObj(obj) || Object.keys(obj).some((k) => !AGENTS.includes(k))) throw new Error('无效的工单模型配置');
  return Object.fromEntries(Object.entries(obj).map(([k, v]) => [k, route(v)]));
}

function options(cat, picked) {
  if (!cat || (cat.available !== undefined && cat.available !== true)) return null;
  const selected = route(picked);
  const match = (rows, id) => {
    const matches = (rows || []).filter((r) => r?.id === id);
    return matches.length === 1 ? matches[0] : null;
  };
  const mode = match(cat.modes, selected.mode);
  const source = match(cat.providers, selected.provider);
  if (
    mode?.available !== true ||
    (mode.providers && (!Array.isArray(mode.providers) || !mode.providers.includes(selected.provider))) ||
    !source ||
    (source.available !== undefined && source.available !== true)
  )
    return null;
  const ids = new Map();
  for (const m of source.models || []) {
    if (ident(m?.id))
      ids.set(m.id, ids.get(m.id) !== false && (m.available === undefined || m.available === true));
  }
  if (!ids.get(selected.model)) return null;
  return {
    provider: selected.provider,
    mode: selected.mode,
    default_model: selected.model,
    models: [
      selected.model,
      ...Array.from(ids)
        .filter(([id, ok]) => ok && id !== selected.model)
        .map(([id]) => id)
    ]
  };
}

function toolsProof(e) {
  if (!isObj(e)) return null;
  if (e.type === 'tool_result' && e.callId)
    return ['ok', 'success', 'completed'].includes(e.status) ? 'ok' : 'attempt';
  if (e.type === 'user' && Array.isArray(e.message?.content)) {
    const blocks = e.message.content.filter((b) => b?.type === 'tool_result' && b.tool_use_id);
    if (blocks.length) return blocks.some((b) => !b.is_error) ? 'ok' : 'attempt';
  }
  if (e.method === 'item/completed') {
    const i = e.params?.item;
    if (['commandExecution', 'mcpToolCall', 'webSearch', 'dynamicToolCall'].includes(i?.type))
      return i.status === 'completed' && (i.exitCode == null || i.exitCode === 0) ? 'ok' : 'attempt';
  }
  const step = e.step_update;
  if (
    e.event === 'step_update' &&
    step?.step_type === 'tool' &&
    ['DONE', 'ERROR', 'FAILED'].includes(step.state)
  )
    return step.state === 'DONE' ? 'ok' : 'attempt';
  return null;
}

function controls(text, continuous) {
  const fields = { SA_GOAL: 'objective', SA_NEXT: 'body', SA_FINISH: 'summary' };
  if (continuous) Object.assign(fields, { SA_LEARN: 'lesson', SA_EXHAUSTED: 'summary' });
  const out = {};
  for (const line of text.split('\n')) {
    const space = line.indexOf(' ');
    const tag = line.slice(0, space);
    if (space < 0 || !Object.hasOwn(fields, tag)) continue;
    const cmd = JSON.parse(line.slice(space + 1));
    if (out[tag] || !isObj(cmd) || !validText(cmd[fields[tag]], 32000))
      throw new Error(tag + ' 缺少有效且唯一的字段');
    if (['SA_EXHAUSTED', 'SA_FINISH'].includes(tag)) {
      if (!['done', 'blocked'].includes(cmd.status)) throw new Error('阶段状态无效');
      if (
        !Array.isArray(cmd.resolved ?? []) ||
        (cmd.resolved || []).some((r) => !Number.isSafeInteger(r?.job) || !validText(r.evidence))
      )
        throw new Error('resolved 必须引用工单中的任务并提供核验依据');
    }
    if (tag === 'SA_EXHAUSTED')
      for (const k of ['checks', 'alternatives']) {
        if (
          !Array.isArray(cmd[k]) ||
          !cmd[k].length ||
          cmd[k].length > 32 ||
          cmd[k].some((v) => !validText(v, 4000))
        )
          throw new Error('耗尽审查需提供 checks 和 alternatives');
      }
    out[tag] = cmd;
  }
  if ((out.SA_FINISH && out.SA_NEXT) || (out.SA_EXHAUSTED && (out.SA_FINISH || out.SA_GOAL || out.SA_NEXT)))
    throw new Error('同一轮的状态指令相互冲突');
  return out;
}

const unresolved = (reports, cmd) =>
  reports
    .filter(
      (r) =>
        ['failed', 'partial', 'cancelled'].includes(r.state) &&
        !(cmd.resolved || []).some((x) => x.job === r.id)
    )
    .map((r) => r.id);

// Called in the same transaction that records the result. A discovery epoch
// cannot reuse evidence or a native session belonging to an older milestone.
function advance(store, job, run, actions, reports, settled, correction) {
  const ticket = job.ticket;
  const { SA_GOAL: goal, SA_LEARN: learn, SA_NEXT: next, SA_FINISH: finish, SA_EXHAUSTED: end } = actions;
  if (learn && learn.lesson !== run.learning) {
    store.run('UPDATE continuous_runs SET learning=? WHERE ticket=?', learn.lesson, ticket);
    store.log(job, 'auto.learn', learn);
  }
  function checkpoint(phase, kind, data) {
    const seq = store.log(job, kind, data);
    store.run(
      `UPDATE continuous_runs SET phase=?, boundary_job=(SELECT max(id) FROM jobs WHERE ticket=?),
      boundary_seq=?,probe=0,success=0,stalls=0,last_signal='',milestone=milestone+? WHERE ticket=?`,
      phase,
      ticket,
      seq,
      Number(kind === 'auto.milestone'),
      ticket
    );
  }
  let hint = correction;
  if (run.phase === 'work') {
    if (goal) {
      store.run('UPDATE tickets SET objective=? WHERE id=?', goal.objective, ticket);
      store.log(job, 'auto.goal', goal);
    }
    if (finish && settled) {
      const missing = unresolved(reports, finish);
      if (finish.status === 'done' && missing.length) hint = '核对失败的子任务并记录解决证据：' + missing;
      else {
        checkpoint('discover', 'auto.milestone', { ...finish, milestone: run.milestone + 1 });
        return { state: 'waiting', reset: true };
      }
    } else if (end) hint = '请先用 SA_FINISH 结束阶段，再实际检查当前项目。';
  } else if (!settled) {
    hint = '等待协作者回报并复核。';
  } else if (!run.probe && (goal || end)) {
    hint = '需要当前探查阶段的实际工具结果。';
  } else if (end) {
    if (end.status === 'done' && (!run.success || unresolved(reports, end).length))
      hint = '成功耗尽需要有效的工具结果，并要核对失败或取消的任务。';
    else {
      store.log(job, 'auto.exhausted', { ...end, last_probe_seq: run.probe, last_success_seq: run.success });
      store.run("UPDATE continuous_runs SET phase='exhausted',stalls=0 WHERE ticket=?", ticket);
      return { state: end.status === 'done' ? 'done' : 'failed' };
    }
  } else if (goal && next && validText(goal.evidence)) {
    const old = store.one(
      `SELECT 1 FROM events WHERE ticket=? AND kind='auto.discovered'
      AND json_extract(payload,'$.objective')=? AND json_extract(payload,'$.evidence')=?`,
      ticket,
      goal.objective,
      goal.evidence
    );
    if (old) hint = '此目标和证据已处理，请用新的事实选择后续工作。';
    else {
      checkpoint('work', 'auto.discovered', {
        objective: goal.objective,
        evidence: goal.evidence,
        body: next.body,
        probe_seq: run.probe
      });
      store.run('UPDATE tickets SET objective=? WHERE id=?', goal.objective, ticket);
      store.log(job, 'auto.next', next);
      return { state: 'waiting', reset: true };
    }
  } else if (goal || finish) hint = '探查阶段需要有 evidence 的 SA_GOAL 和 SA_NEXT，或完整的 SA_EXHAUSTED。';
  const step = hint
    ? { body: hint }
    : next || { body: settled ? '请依据当前阶段和实际结果继续。' : '等待协作者回报并复核。' };
  const stalls = settled && step.body === run.last_signal ? run.stalls + 1 : 0;
  store.run('UPDATE continuous_runs SET stalls=?,last_signal=? WHERE ticket=?', stalls, step.body, ticket);
  if (stalls) {
    const delay = Math.min(300, 2 ** Math.min(stalls, 9));
    store.run('UPDATE jobs SET ready_after=? WHERE id=?', Date.now() / 1000 + delay, job.id);
    store.log(job, 'auto.backoff', { reason: '重复的下一步，退避后重新评估。', after_seconds: delay });
  }
  store.log(job, 'auto.next', step);
  return { state: 'waiting' };
}

module.exports = {
  AGENTS,
  PROFILES,
  TERMINAL,
  isFull,
  canWrite,
  validText,
  isObj,
  ident,
  digest,
  childProfiles,
  route,
  routes,
  options,
  toolsProof,
  controls,
  unresolved,
  advance
};
