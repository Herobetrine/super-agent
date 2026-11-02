'use strict';
const { setImmediate: yieldIO } = require('node:timers/promises');
const { createActivity, applyActivity } = require('./native-activity.cjs');

// These rows retain only operational labels. Agent text, arguments, credentials,
// thinking deltas and command output stay out of the progress projection.
const stages = new Map(
  Object.entries({
    user: ['已提交目标', 'run'],
    'user.follow': ['已追加指令', 'run'],
    'turn.admitted': ['调度器已接单', 'run'],
    'route.requested': ['已选择执行端和模型', 'run'],
    input: ['已准备本轮上下文', 'run'],
    running: ['已开始执行本轮', 'run'],
    'adapter.permission': ['已应用执行权限', 'run'],
    'adapter.route': ['已绑定执行路由', 'run'],
    files: ['附件快照已就绪', 'run'],
    send: ['已派发协作任务', 'task'],
    reply: ['本轮已返回答复', 'run', 'completed'],
    role: ['已更新任务分工', 'task'],
    note: ['已投递定向留言', 'task'],
    'note.accepted': ['执行端已接收留言', 'task', 'completed'],
    'note.rejected': ['留言投递被拒绝', 'task', 'failed'],
    retry: ['任务已安排重试', 'run'],
    'approval.request': ['等待操作审批', 'approval'],
    'approval.decision': ['审批已处理', 'approval', 'completed'],
    'approval.unattended_decline': ['无人值守审批已拒绝', 'approval', 'failed'],
    'auto.goal': ['已更新阶段目标', 'task'],
    'auto.next': ['已安排下一步', 'task'],
    'auto.learn': ['已记录工作经验', 'task', 'completed'],
    'auto.milestone': ['已记录阶段结果', 'task', 'completed'],
    'auto.discovered': ['已发现新的工作', 'task'],
    'auto.exhausted': ['已提交穷尽检查', 'task', 'completed'],
    'auto.finish': ['已提交阶段结论', 'task', 'completed'],
    'auto.recover': ['正在复核中断状态', 'task'],
    'auto.retry': ['已安排延后重试', 'task'],
    'auto.backoff': ['等待退避后继续', 'task'],
    'auto.protocol': ['控制指令需要修正', 'task'],
    'routes.changed': ['已更新工单模型绑定', 'run'],
    failed: ['任务执行失败', 'run', 'failed'],
    cancelled: ['任务已取消', 'run', 'stopped'],
    partial: ['任务部分完成', 'task'],
    interrupted: ['任务中断，等待处理', 'run', 'stopped'],
    draining: ['等待现有子任务结束', 'run'],
    'mcp.result': ['已收到 MCP 工具回执', 'tool', 'completed'],
    'plan.add': ['已添加计划项', 'task'],
    'plan.done': ['已勾选计划项', 'task', 'completed'],
    'plan.reopen': ['已重新打开计划项', 'task']
  })
);
function platformActivity(row) {
  if (row.kind === 'job.progress' && typeof row.summary === 'string')
    return { category: 'report', state: 'reported', label: '进展汇报 · ' + row.summary };
  if (row.kind === 'mcp.started')
    return { category: 'tool', state: 'running', label: '正在调用已授权的 MCP 工具' };
  const parts = stages.get(row.kind);
  if (!parts) return null;
  let [label, category, state = 'running'] = parts;
  if (row.status === 'deny') {
    label = '本次操作未获批准';
    state = 'stopped';
  }
  if (row.kind === 'mcp.result' && row.status !== 'done') {
    label = 'MCP 工具调用未成功';
    state = 'failed';
  }
  if ((row.kind === 'auto.milestone' || row.kind === 'auto.exhausted') && row.status === 'blocked') {
    label = '已记录阻塞原因';
    state = 'stopped';
  }
  return { label, category, state };
}
class ProgressJournal {
  constructor(db) {
    this.db = db;
    this.seen = new Map();
    // A derived per-connection TEMP table can be populated even when the main
    // ticket database is opened read-only by an observer. No user data migration.
    db.exec(`CREATE TEMP TABLE IF NOT EXISTS progress_view(seq INTEGER PRIMARY KEY,ticket INTEGER,job INTEGER,turn INTEGER,ts REAL,
      agent TEXT,original TEXT,payload TEXT); CREATE INDEX IF NOT EXISTS temp.progress_ticket_seq ON progress_view(ticket,seq)`);
    this.insert = db.prepare('INSERT OR REPLACE INTO temp.progress_view VALUES(?,?,?,?,?,?,?,?)');
    this.rows = db.prepare(`SELECT e.seq,e.job,e.turn,e.ts,e.kind,j.agent,
      CASE WHEN e.kind='native' THEN e.payload END payload,
      CASE WHEN e.kind='job.progress' THEN json_extract(e.payload,'$.summary') END summary,
      CASE WHEN e.kind='mcp.result' THEN json_extract(e.payload,'$.state')
           WHEN e.kind IN ('auto.milestone','auto.exhausted') THEN json_extract(e.payload,'$.status')
           WHEN e.kind='approval.decision' THEN json_extract(e.payload,'$.decision') END status
      FROM events e LEFT JOIN jobs j ON j.id=e.job WHERE e.ticket=? AND e.seq>? AND e.seq<=?
      AND e.kind NOT IN ('delta','message.boundary') ORDER BY e.seq LIMIT 64`);
  }
  async ensure(ticket, end) {
    let entry = this.seen.get(ticket);
    if (!entry) {
      entry = { seq: 0, native: createActivity() };
      this.seen.set(ticket, entry);
    }
    while (entry.seq < end) {
      if (!entry.loading)
        entry.loading = this.build(ticket, entry, end).finally(() => {
          entry.loading = null;
        });
      await entry.loading;
    }
    return entry.native;
  }
  async build(ticket, entry, end) {
    while (entry.seq < end) {
      const rows = this.rows.all(ticket, entry.seq, end);
      for (const row of rows) {
        const item = row.kind === 'native' ? applyActivity(entry.native, row) : platformActivity(row);
        if (item)
          this.insert.run(
            row.seq,
            ticket,
            row.job,
            row.turn,
            row.ts,
            row.agent,
            row.kind,
            JSON.stringify(item)
          );
        entry.seq = row.seq;
      }
      if (rows.length < 64) {
        entry.seq = end;
        break;
      }
      await yieldIO();
    }
  }
  read(ticket, after, end, reverse, limit, except = []) {
    const excluding = except.length ? ` AND original NOT IN (${except.map(() => '?').join(',')})` : '';
    const rows = this.db
      .prepare(
        `SELECT seq,ts,job,turn,agent,payload FROM temp.progress_view WHERE ticket=? AND seq>? AND seq<=?
      ${excluding} ORDER BY seq ${reverse ? 'DESC' : 'ASC'} LIMIT ?`
      )
      .all(ticket, after, end, ...except, limit);
    return rows.map((r) => ({ ...r, kind: 'activity', payload: JSON.parse(r.payload) }));
  }
}
module.exports = { ProgressJournal };
