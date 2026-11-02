'use strict';
const StudioActivity = (() => {
  const { agents, el, compact, button, dialog, closeDialog, invoke, activityTime } = StudioUI;
  const states = {
    running: '执行中',
    started: '已开始',
    done: '已完成',
    completed: '已完成',
    success: '成功',
    failed: '失败',
    error: '失败',
    stopped: '已停止',
    cancelled: '已停止',
    queued: '等待执行',
    waiting: '等待中',
    interrupted: '已中断',
    reported: '已汇报',
    unknown: '状态未知'
  };
  function nameOf(e, jobs) {
    const agent = e.agent || e.payload.agent || jobs.find((j) => j.id === e.job)?.agent;
    return agents[agent] || agent || '';
  }
  function title(e) {
    return compact(e.payload.label || '执行事件', 200);
  }
  function report(e, jobs) {
    const card = el('article', 'entry progress-report');
    const owner = [
      nameOf(e, jobs),
      e.job ? '任务 #' + e.job : '',
      e.turn ? '第 ' + e.turn + ' 轮' : '',
      activityTime(e.ts)
    ];
    card.append(
      el('strong', '', '阶段汇报'),
      el('span', 'small-note', owner.filter(Boolean).join(' · ')),
      el('div', 'report-text', String(e.payload.label || '').replace(/^进展汇报\s*·\s*/, ''))
    );
    return card;
  }
  function row(e, jobs) {
    const p = e.payload;
    const detail = el('details', 'progress-item');
    const header = el('summary');
    header.append(el('time', '', activityTime(e.ts)), document.createTextNode(' ' + title(e)));
    detail.append(header);
    const owner = [
      nameOf(e, jobs),
      e.job ? '平台任务 #' + e.job : '',
      e.turn ? '第 ' + e.turn + ' 轮' : ''
    ].filter(Boolean);
    if (p.state) owner.push(states[p.state] || compact(p.state, 40));
    if (p.tool) owner.push('工具 ' + compact(p.tool, 80));
    if (p.taskId) owner.push('内部任务 ' + compact(p.taskId, 80));
    detail.append(el('p', 'small-note', owner.join(' · ')));
    if (p.label) detail.append(el('div', 'report-text', p.label));
    if (p.title) detail.append(el('div', 'small-note', p.title));
    if (Array.isArray(p.steps) && p.steps.length) {
      const steps = el('ul', 'progress-steps');
      for (const step of p.steps)
        steps.append(el('li', '', [step.tool, states[step.state] || '状态未知'].filter(Boolean).join(' · ')));
      detail.append(steps);
    }
    return detail;
  }
  function group() {
    const element = el('details', 'entry progress-group');
    const summary = el('summary'),
      list = el('div');
    element.append(summary, list);
    const records = [];
    element.ontoggle = () => {
      list.replaceChildren();
      if (element.open) for (const r of records) list.append(row(r, []));
    };
    return {
      element,
      get count() {
        return records.length;
      },
      add(e, jobs) {
        const actor = nameOf(e, jobs);
        const record = { ...e, agent: actor };
        records.push(record);
        if (element.open) list.append(row(record, []));
        summary.textContent =
          records.length + ' 条进展 · ' + activityTime(e.ts) + ' ' + actor + ' ' + title(e);
      }
    };
  }
  function history(ticket, jobs) {
    if (!ticket) return;
    const d = dialog('进展记录');
    d.body.classList.add('progress-history');
    let cursors = [null];
    async function page(nextCursors) {
      const before = nextCursors.at(-1);
      d.body.replaceChildren(el('p', 'small-note', '正在读取进展…'));
      d.foot.replaceChildren(button('关闭', closeDialog, 'quiet'));
      try {
        const result = await invoke('activityPage', before ? { ticket, before } : { ticket });
        if (!d.alive()) return;
        cursors = nextCursors;
        const events = [...result.events].sort((a, b) => b.seq - a.seq);
        d.body.replaceChildren(
          el('p', 'small-note', '第 ' + cursors.length + ' 页 · 最新进展在前，展开可查看来源和状态。')
        );
        if (!events.length) d.body.append(el('p', 'empty', '尚无已记录的进展。'));
        for (const e of events) d.body.append(row(e, jobs));
        const controls = [];
        if (cursors.length > 1) controls.push(button('较新一页', () => page(cursors.slice(0, -1)), 'quiet'));
        if (result.more) controls.push(button('较早一页', () => page([...cursors, result.next]), 'quiet'));
        d.foot.replaceChildren(
          button('回到最新', () => page([null]), 'quiet'),
          ...controls,
          button('关闭', closeDialog, 'quiet')
        );
        d.body.scrollTop = 0;
        d.body.closest('dialog').scrollTop = 0;
      } catch (e) {
        if (!d.alive()) return;
        d.body.replaceChildren(
          el('p', 'callout danger', e.message),
          button('重试', () => page(nextCursors), 'quiet')
        );
      }
    }
    void page(cursors);
  }
  return { group, history, title, report };
})();
