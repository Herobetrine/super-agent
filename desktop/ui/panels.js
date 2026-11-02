'use strict';
const StudioPanels = (() => {
  const {
    $,
    el,
    button,
    icon,
    invoke,
    notice,
    report,
    dialog,
    closeDialog,
    agents,
    states,
    profiles,
    compact,
    activityTime
  } = StudioUI;
  let app,
    tab = 'tasks',
    lastKey = '',
    diffText = '',
    diffProject = '',
    diffBusy = false,
    diffRun = 0;
  let page = null,
    cursors = [],
    pageBusy = false;
  const expandedTasks = new Set();
  function init(controller) {
    app = controller;
    for (const t of $('tabs').children) t.onclick = () => select(t.dataset.tab);
    $('close-inspector').onclick = () => toggle(false);
    $('toggle-inspector').onclick = () => toggle($('inspector').hidden);
  }
  function toggle(open) {
    $('inspector').hidden = !open;
    $('shell').classList.toggle('inspect-open', open);
    $('toggle-inspector').setAttribute('aria-expanded', String(open));
    if (open) draw(true);
  }
  function select(t) {
    tab = t;
    toggle(true);
    if (t === 'diff' && app.cwd() && diffProject !== app.cwd()) void loadDiff();
  }
  function reset() {
    expandedTasks.clear();
    page = null;
    cursors = [];
    pageBusy = false;
    diffText = '';
    diffProject = '';
    diffBusy = false;
    ++diffRun;
    lastKey = '';
    draw(true);
  }
  async function loadPage(nextCursors) {
    const ticket = app.ticket()?.id;
    if (!ticket || pageBusy) return;
    pageBusy = true;
    draw(true);
    try {
      const before = nextCursors.at(-1);
      const result = await invoke('jobsPage', before ? { ticket, before } : { ticket });
      if (ticket !== app.ticket()?.id) return;
      cursors = nextCursors;
      page = result;
      expandedTasks.clear();
      $('inspector-body').scrollTop = 0;
    } catch (e) {
      if (app.ticket()?.id === ticket) report(e);
    } finally {
      if (ticket === app.ticket()?.id) {
        pageBusy = false;
        draw(true);
      }
    }
  }
  function taskNavigation(total, count) {
    const nav = el('div', 'task-nav');
    nav.append(
      el(
        'p',
        'small-note',
        (page ? '第 ' + cursors.length + ' 页' : '任务概览') +
          ' · 显示 ' +
          count +
          ' / 共 ' +
          total +
          ' 个任务'
      )
    );
    const controls = el('div', 'head-actions');
    if (page) {
      controls.append(
        button(
          '概览',
          () => {
            page = null;
            cursors = [];
            expandedTasks.clear();
            draw(true);
          },
          'quiet'
        )
      );
      if (cursors.length > 1)
        controls.append(button('上一页', () => loadPage(cursors.slice(0, -1)), 'quiet'));
      if (page.more) controls.append(button('下一页', () => loadPage([...cursors, page.next]), 'quiet'));
      controls.append(button('刷新本页', () => loadPage(cursors), 'quiet'));
    } else if (count < total)
      controls.append(button('分页浏览全部任务', () => loadPage([null]), 'quiet', 'layers'));
    for (const b of controls.children) b.disabled = pageBusy;
    nav.append(controls);
    return nav;
  }
  function nativeTasks(tasks) {
    const section = el('section', 'native-tasks');
    section.setAttribute('aria-label', 'Claude 内部任务');
    section.append(el('h3', 'task-section-title', 'Claude 内部任务'));
    if (!tasks.length) {
      section.append(el('p', 'small-note', '尚未收到内部任务事件。'));
      return section;
    }
    const names = {running: '执行中', done: '已完成', completed: '已完成', failed: '失败', stopped: '已停止', unknown: '状态未知'};
    for (const task of tasks) {
      const row = el('article', 'native-task');
      row.append(el('strong', '', compact(task.label || '内部任务', 120)),
        el('span', 'tag ' + (task.state === 'failed' ? 'failed' : ''), names[task.state] || names.unknown));
      if(task.title)row.append(el('div','',task.title));
      const meta = ['平台任务 #' + task.job, '第 ' + task.turn + ' 轮'];
      if (task.id) meta.push('内部任务 ' + compact(task.id, 48));
      if (Number.isSafeInteger(task.toolUses) && task.toolUses >= 0) meta.push('已用工具 ' + task.toolUses + ' 次');
      if (task.lastTool) meta.push('最近工具 ' + compact(task.lastTool, 80));
      if (activityTime(task.ts)) meta.push(activityTime(task.ts));
      row.append(el('div', 'small-note', meta.join(' · ')));
      section.append(row);
    }
    return section;
  }
  async function loadDiff() {
    const path = app.cwd();
    if (!path) {
      notice('请先选择项目。');
      return;
    }
    const run = ++diffRun;
    diffProject = path;
    diffBusy = true;
    draw(true);
    try {
      const d = await invoke('diff', { cwd: path });
      if (run === diffRun) diffText = String(d?.text || '工作区没有可显示的差异。');
    } catch (e) {
      if (run === diffRun) diffText = e.message || '读取 Git 变更失败';
    } finally {
      if (run === diffRun) {
        diffBusy = false;
        draw(true);
      }
    }
  }
  function draw(force = false) {
    if (!app || $('inspector').hidden) return;
    const snap = app.snapshot(),
      ticket = app.ticket(),
      box = $('inspector-body');
    for (const t of $('tabs').children) {
      t.classList.toggle('active', t.dataset.tab === tab);
      t.setAttribute('aria-selected', String(t.dataset.tab === tab));
    }
    const preview = snap?.jobs || [],
      total = snap?.totalJobs ?? preview.length,
      plan = snap?.plan || [],
      approvals = snap?.approvals || [];
    const fresh = new Map(preview.map((job) => [job.id, job]));
    const jobs = page ? page.jobs.map((job) => fresh.get(job.id) || job) : preview;
    const key =
      tab === 'tasks'
        ? JSON.stringify([tab, ticket?.id, total, jobs, approvals, snap?.nativeActivity?.tasks])
        : tab === 'plan'
          ? JSON.stringify([tab, ticket?.id, plan])
          : JSON.stringify([tab, app.cwd(), diffBusy, diffText]);
    if (!force && key === lastKey) return;
    lastKey = key;
    const draft = box.querySelector('.plan-form input')?.value || '';
    const offset = box.scrollTop;
    box.replaceChildren();
    if (tab === 'diff') {
      const head = el('div', 'head-actions');
      head.append(button('读取 Git 差异', loadDiff, 'quiet', 'refresh'));
      box.append(head);
      box.append(el('p', 'small-note', '当前项目的只读差异预览。'));
      if (!app.cwd()) {
        box.append(el('div', 'empty', '选择项目后，在此检查 Git 变更。'));
        return;
      }
      if (diffBusy) {
        box.append(el('p', 'empty', '正在读取变更…'));
        return;
      }
      if (!diffText || diffProject !== app.cwd()) return;
      const pre = el('pre', 'diff-text'),
        truncated = diffText.length > 160000;
      for (const l of diffText.slice(0, 160000).split('\n')) {
        pre.append(
          el('span', l.startsWith('+') ? 'diff-add' : l.startsWith('-') ? 'diff-del' : '', l || ' ')
        );
      }
      box.append(pre);
      if (truncated) box.append(el('p', 'small-note', '变更较长，当前显示前 160,000 个字符。'));
      return;
    }
    if (!ticket) {
      box.append(
        el(
          'p',
          'empty',
          tab === 'tasks' ? '发出首条消息后，在这里查看协作分工。' : '发出首条消息后，可为这个会话建立计划。'
        )
      );
      return;
    }
    if (tab === 'plan') {
      box.append(el('p', 'small-note', '这份清单属于当前会话。勾选项目可更新完成状态。'));
      for (const item of plan) {
        const row = el('label', 'plan-row' + (item.done ? ' done' : ''));
        const c = el('input');
        c.type = 'checkbox';
        c.checked = Boolean(item.done);
        c.onchange = async () => {
          c.disabled = true;
          try {
            await invoke('planToggle', { ticket: ticket.id, id: item.id });
            await app.refresh();
          } catch (e) {
            c.checked = Boolean(item.done);
            report(e);
          } finally {
            c.disabled = false;
          }
        };
        row.append(c, el('span', '', item.body));
        box.append(row);
      }
      const form = el('form', 'plan-form'),
        input = el('input');
      input.value = draft;
      input.placeholder = '添加计划项';
      input.setAttribute('aria-label', '添加计划项');
      const submit = el('button', 'quiet', '添加');
      submit.type = 'submit';
      form.append(input, submit);
      form.onsubmit = async (e) => {
        e.preventDefault();
        if (!input.value.trim() || submit.disabled) return;
        submit.disabled = true;
        try {
          await invoke('planAdd', { ticket: ticket.id, body: input.value });
          input.value = '';
          await app.refresh();
        } catch (err) {
          report(err);
        } finally {
          submit.disabled = false;
        }
      };
      box.append(form);
    } else {
      if (approvals.length && !app.observing)
        box.append(button('处理待审批请求 · ' + approvals.length, showApprovals, 'danger compact', 'shield'));
      const native = snap?.nativeActivity?.tasks || [];
      if (native.length || ticket.lead === 'claude' || preview.some(j => j.agent === 'claude'))
        box.append(nativeTasks(native));
      box.append(el('h3', 'task-section-title', '平台工单任务'));
      box.append(taskNavigation(total, jobs.length));
      if (!jobs.length) box.append(el('p', 'empty', '正在等候首个任务。'));
      for (const job of jobs) {
        const card = el('details', 'task-card'),
          summary = el('summary');
        card.open = expandedTasks.has(job.id);
        card.addEventListener('toggle', () => {
          if (card.open) expandedTasks.add(job.id);
          else expandedTasks.delete(job.id);
        });
        summary.append(
          el('strong', '', agents[job.agent] || job.agent),
          el('span', 'tag ' + (job.state === 'failed' ? 'failed' : ''), states[job.state] || job.state),
          el(
            'span',
            'task-role',
            '#' + job.id + ' · ' + compact(job.role || (!job.parent ? '主控' : '协作者'), 50)
          )
        );
        card.append(
          summary,
          el(
            'p',
            'secondary',
            '#' +
              job.id +
              ' · ' +
              (job.role || (!job.parent ? '主控' : '协作者')) +
              ' · 第 ' +
              (job.turn || 1) +
              ' 轮'
          )
        );
        if (job.body) card.append(el('p', '', String(job.body).slice(0, 1200)));
        const permission = job.permission_override || ticket.permission_profile;
        card.append(el('p', 'small-note', '权限 · ' + (profiles[permission] || permission || '沿用工单')));
        const route = job.route || job.bound_route || job.route_override;
        if (route) {
          let r = route;
          if (typeof r === 'string') {
            try {
              r = JSON.parse(r);
            } catch {
              r = null;
            }
          }
          if (r)
            card.append(
              el('p', 'small-note', '模型 · ' + [r.provider, r.model, r.mode].filter(Boolean).join(' / '))
            );
        }
        if (job.result) card.append(el('p', '', StudioUI.cleanReply(job.result).slice(0, 1600)));
        const actions = el('div', 'head-actions');
        if (!app.observing && ['running', 'queued', 'waiting'].includes(job.state))
          actions.append(button('定向留言', () => app.target(job), 'quiet', 'chat'));
        if (!app.observing && ['failed', 'interrupted'].includes(job.state) && ticket.state !== 'cancelled') {
          actions.append(
            button(
              '重试',
              async (e) => {
                const b = e.currentTarget;
                b.disabled = true;
                try {
                  await invoke('retry', { job: job.id });
                  await app.refresh();
                } catch (err) {
                  report(err);
                } finally {
                  b.disabled = false;
                }
              },
              'quiet',
              'refresh'
            )
          );
        }
        card.append(actions);
        box.append(card);
      }
    }
    box.scrollTop = offset;
  }
  function showApprovals() {
    const ticket = app.ticket(),
      rows = app.snapshot()?.approvals || [];
    if (!ticket || !rows.length) {
      notice('当前会话没有待审批请求。');
      return;
    }
    const d = dialog('待审批请求');
    for (const r of rows) {
      const card = el('article', 'approval');
      card.append(el('strong', '', '任务 #' + r.job + ' · 请求 #' + r.id));
      card.append(
        el('pre', 'raw', typeof r.action === 'string' ? r.action : JSON.stringify(r.action, null, 2))
      );
      const actions = el('div', 'head-actions');
      async function decide(allow) {
        for (const b of actions.children) b.disabled = true;
        try {
          await invoke('decide', { id: r.id, allow });
          if (d.alive()) closeDialog();
          notice(allow ? '已允许本次请求。' : '已拒绝本次请求。');
          await app.refresh();
        } catch (e) {
          report(e);
          for (const b of actions.children) b.disabled = false;
        }
      }
      actions.append(
        button('拒绝', () => decide(false), 'quiet'),
        button('允许本次请求', () => decide(true), 'danger compact')
      );
      card.append(actions);
      d.body.append(card);
    }
    d.foot.append(button('关闭', closeDialog, 'quiet'));
  }
  return { init, toggle, select, draw, reset, showApprovals };
})();
