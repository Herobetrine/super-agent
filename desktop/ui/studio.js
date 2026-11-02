'use strict';
(() => {
  const {
    $,
    el,
    icon,
    button,
    opt,
    field,
    notice,
    report,
    invoke,
    dialog,
    closeDialog,
    ask,
    textView,
    cleanReply,
    compact,
    basename,
    stamp,
    activityTime,
    agents,
    states,
    profiles
  } = StudioUI;
  const drafts = new Map(),
    recent = new Map();
  const observeParam = new URLSearchParams(location.search).get('observe');
  const observing = observeParam !== null;
  const eventLabels = {
    delta: '收到文本输出', reply: '收到回报', send: '分派任务', role: '记录分工',
    'approval.request': '请求审批', 'approval.decision': '审批已处理',
    'mcp.result': '收到 MCP 工具回执', failed: '任务失败', interrupted: '任务中断',
    'auto.retry': '安排自动重试', 'auto.backoff': '等待重试', 'auto.milestone': '收到阶段回报'
  };
  let current = null,
    snapshot = null,
    view = emptyView(),
    defaults = {},
    historyKey = '',
    historyRun = 0;
  let project = '',
    lead = 'deepseek',
    profile = 'readonly',
    allowRouting = true,
    draftRoutes = null,
    draftExtensions = null,
    target = null,
    attachments = [];
  let sending = false, stopping = false,
    fileRun = 0,
    fileTimer = 0,
    fileIndex = 0,
    fileRows = [],
    mention = null,
    commandIndex = 0,
    commandRows = [];
  function emptyView() {
    return {
      cursor: 0,
      entries: new Map(),
      streams: new Map(),
      request: null,
      goal: false,
      rootReply: false,
      latestEvent: null,
      progressGroup: null
    };
  }
  function ticket() {
    return snapshot?.ticket || null;
  }
  function cwd() {
    return ticket()?.cwd || (current == null ? project : '');
  }
  const active = (s) => ['queued', 'running', 'waiting', 'draining'].includes(s);
  function rootJob() {
    return snapshot?.jobs?.find((job) => !job.parent);
  }
  function state() {
    return ticket()?.state || rootJob()?.state;
  }
  function routeMap(raw) {
    if (typeof raw === 'string') {
      try {
        return JSON.parse(raw) || {};
      } catch {
        return {};
      }
    }
    return raw || {};
  }
  function draftKey() {
    return String(current ?? 'new') + ':' + String(target?.id ?? 'main');
  }
  function saveDraft() {
    drafts.set(draftKey(), { body: $('editor').value, files: [...attachments] });
  }
  function restoreDraft() {
    const d = drafts.get(draftKey());
    $('editor').value = d?.body || '';
    attachments = [...(d?.files || [])];
    renderComposer();
  }
  function connected(ok) {
    $('connection-dot').className = ok ? 'ready' : 'failed';
    $('connection').textContent = ok ? '本地调度器已连接' : '调度器未连接';
  }
  function openRail() {
    const shell = $('shell');
    if (window.innerWidth <= 720) {
      shell.classList.toggle('mobile-rail');
      shell.classList.remove('rail-off');
    } else if (window.innerWidth <= 1050 && shell.classList.contains('inspect-open')) {
      StudioPanels.toggle(false);
      shell.classList.remove('rail-off');
    } else shell.classList.toggle('rail-off');
  }
  function newChat() {
    closeModelMenu();
    if (current == null && !target) {
      $('editor').focus();
      return;
    }
    saveDraft();
    current = null;
    snapshot = null;
    view = emptyView();
    target = null;
    profile = 'readonly';
    allowRouting = true;
    draftRoutes = null;
    draftExtensions = null;
    closeFiles();
    $('feed').replaceChildren();
    $('shell').classList.remove('mobile-rail');
    StudioPanels.reset();
    StudioPanels.toggle(false);
    notice('');
    restoreDraft();
    renderHeader();
    renderHistory();
    $('editor').focus();
    if (!observing) document.dispatchEvent(new CustomEvent('studio:ticket-changed', { detail: current }));
  }
  async function openChat(id, recordDraft = true) {
    closeModelMenu();
    if (id === current) {
      $('shell').classList.remove('mobile-rail');
      return;
    }
    if (recordDraft) saveDraft();
    current = id;
    snapshot = null;
    view = emptyView();
    target = null;
    closeFiles();
    $('feed').replaceChildren();
    $('shell').classList.remove('mobile-rail');
    StudioPanels.reset();
    notice('');
    restoreDraft();
    renderHeader();
    renderHistory();
    await refresh();
    $('editor').focus();
    if (id === current && !observing) document.dispatchEvent(new CustomEvent('studio:ticket-changed', { detail: current }));
  }
  function renderHistory() {
    const rows = [...recent.values()],
      box = $('history');
    const key = JSON.stringify([current, $('history-search').value, rows.map((r) => [r.id, r.goal, r.cwd, r.state, r.created])]);
    if (key === historyKey) return;
    historyKey = key;
    const offset = box.scrollTop;
    box.replaceChildren();
    for (const r of rows) {
      const b = el('button', r.id === current ? 'active' : '');
      b.type = 'button';
      b.dataset.ticketId = r.id;
      if (r.id === current) b.setAttribute('aria-current', 'page');
      b.title = r.goal || '会话';
      b.append(
        el('strong', '', compact(r.goal || '会话', 100)),
        el('small', '', basename(r.cwd) + ' · ' + (states[r.state] || r.state || ''))
      );
      b.onclick = () => void openChat(r.id).catch(report);
      box.append(b);
    }
    if (!rows.length)
      box.append(el('div', 'empty', $('history-search').value ? '没找到匹配的会话。' : '还没有会话。'));
    if (!observing && typeof Workbench !== 'undefined') Workbench.organizeHistory(box, rows);
    box.scrollTop = offset;
  }
  async function history() {
    const run = ++historyRun;
    const rows = await invoke('history', { q: $('history-search').value });
    if (run !== historyRun) return;
    recent.clear();
    for (const r of rows) recent.set(r.id, r);
    connected(true);
    renderHistory();
  }
  function renderActivity(status) {
    const live = $('live-activity');
    live.hidden = !snapshot || !active(status);
    if (live.hidden) return;
    const leader = rootJob();
    const running = new Map();
    for (const job of snapshot.jobs) {
      if (job.state !== 'running' || job.id === leader?.id) continue;
      const name = agents[job.agent] || job.agent;
      running.set(name, (running.get(name) || 0) + 1);
    }
    const parts = ['主控 ' + (agents[ticket().lead] || ticket().lead) + ' · ' + (states[status] || status)];
    if (running.size) {
      const peers = [...running].map(([name,count]) => name + (count > 1 ? ' ×' + count : ''));
      const partial = snapshot.totalJobs > snapshot.jobs.length;
      parts.push((partial ? '概览中执行的协作者 · ' : '执行中的协作者 · ') + peers.join('、'));
    }
    $('live-agents').textContent = parts.join('　/　');
    const native = snapshot.nativeActivity?.latest;
    const last = native && (!view.latestEvent || native.ts >= view.latestEvent.ts) ? native : view.latestEvent;
    let detail = status === 'queued' ? '等待调度' : '等待首个执行事件';
    if (last) {
      const source = snapshot.jobs.find(j => j.id === last.job);
      const agent = source?.agent || last.agent;
      detail = ['最近活动', [agents[agent] || agent, last.job ? '#' + last.job : ''].filter(Boolean).join(' '),
        last.turn ? '第 ' + last.turn + ' 轮' : '', compact(last.label, 160)].filter(Boolean).join(' · ');
    }
    $('live-detail').textContent = detail;
    $('live-detail').title = detail;
    $('live-time').textContent = last ? activityTime(last.ts) : '';
  }
  function renderHeader() {
    if (typeof Workbench !== 'undefined' && !observing) Workbench.update(snapshot, tab => StudioPanels.select(tab));
    const t = ticket(),
      name = t?.goal || recent.get(current)?.goal || '新对话';
    $('chat-title').textContent = compact(name, 120);
    $('chat-title').title = name;
    $('welcome').hidden = current !== null;
    $('activity-history').hidden = !t;
    const status = state() || (current == null ? 'draft' : 'loading'),
      badge = $('state-badge');
    renderActivity(status);
    badge.textContent = states[status] || { draft: '草稿', loading: '载入中' }[status] || status;
    badge.className = 'badge ' + (status === 'running' ? 'running' : status === 'failed' ? 'failed' : '');
    const approvals = snapshot?.approvals || [];
    $('approval-shortcut').hidden = !approvals.length;
    $('approval-count').textContent = '待审批 ' + approvals.length;
    const auto = t?.permission_profile === 'auto';
    $('auto-strip').hidden = !auto;
    if (auto) {
      const run = snapshot.run,
        stage =
          status === 'cancelled'
            ? '已停止 · 需要明确恢复'
            : run?.phase === 'exhausted'
              ? '已提交穷尽说明'
              : run?.phase === 'discover'
                ? '正在搜寻下一步'
                : active(status)
                  ? run
                    ? '执行阶段 ' + (Number(run.milestone || 0) + 1)
                    : '执行目标'
                  : states[status] || status;
      $('auto-phase').textContent = '自动运行　/　' + stage;
      $('auto-goal').textContent = '当前目标 · ' + (t.objective || t.goal);
      $('auto-goal').title = t.objective || t.goal;
      $('auto-learning').textContent = '最近经验 · ' + (run?.learning || '暂无记录');
      $('auto-learning').title = run?.learning || '';
    }
  }
  const defaultModels = new Map();
  async function readDefaultModel(agent) {
    if (observing) return;
    let entry = defaultModels.get(agent);
    if (!entry) {
      entry = {};
      defaultModels.set(agent, entry);
    }
    if (entry.pending || Date.now() < (entry.nextCheck || 0)) return;
    entry.pending = true;
    try {
      const revision = await invoke('catalogRevision', { agent });
      if (revision === entry.revision) return;
      const catalog = await invoke('catalog', { agent });
      entry.revision = revision;
      entry.route = catalog?.default;
    } catch {
      delete entry.route;
      delete entry.revision;
    } finally {
      entry.pending = false;
      entry.nextCheck = Date.now() + 10000;
      if ((ticket()?.lead || lead) === agent) renderComposer();
    }
  }
  function renderComposer() {
    const t = ticket(),
      fixed = current !== null,
      stopped = t?.permission_profile === 'auto' && state() === 'cancelled';
    const perm = fixed ? t?.permission_profile || 'legacy' : profile,
      picker = $('profile');
    if (![...picker.options].some((o) => o.value === perm))
      opt(picker, perm, '旧工单 · ' + (t?.writes ? '受控写入' : '只读'));
    if (!fixed) for (const o of [...picker.options]) if (!profiles[o.value]) o.remove();
    picker.value = perm;
    picker.disabled = fixed || sending;
    picker.classList.toggle('risky', perm === 'auto' || perm === 'full');
    picker.title = fixed ? '权限已锁定 · 新建对话可更改' : '选择会话权限';
    const dir = cwd(),
      ap = t?.lead || lead;
    $('project-button').querySelector('span').textContent = dir ? basename(dir) : '选择项目';
    $('project-button').title = dir || '选择项目目录';
    $('session-settings').querySelector('span').textContent = '主控 · ' + (agents[ap] || ap || '载入中');
    $('attach-button').disabled = fixed || !dir || sending;
    $('attach-button').title = fixed ? '项目文件可在新会话首条消息中附加' : '选择项目上下文文件';
    const chosen = rootJob()?.route || routeMap(t?.agent_routes || draftRoutes || defaults)[ap];
    if (!chosen && !observing) void readDefaultModel(ap);
    const displayRoute = chosen || defaultModels.get(ap)?.route;
    const modelButton = $('model-button');
    modelButton.querySelector('span').textContent = chosen?.model || (displayRoute?.model ? displayRoute.model + ' · 默认' : '跟随默认');
    const modelName = [agents[ap] || ap, displayRoute?.provider, displayRoute?.model || '跟随本机默认模型'].filter(Boolean).join(' / ');
    modelButton.title = '主控模型 · ' + modelName;
    modelButton.setAttribute('aria-label', '选择主控模型 · ' + modelName);
    modelButton.disabled = (fixed && !t) || sending;
    const to = $('recipient');
    to.hidden = !target;
    to.replaceChildren();
    if (target)
      to.append(
        button(
          '仅发给 ' + (agents[target.agent] || target.agent) + ' #' + target.id,
          () => setTarget(null),
          '',
          'close'
        )
      );
    $('attachments').replaceChildren();
    for (const path of attachments) {
      const b = button(
        basename(path),
        () => {
          attachments = attachments.filter((f) => f !== path);
          renderComposer();
        },
        '',
        'close'
      );
      b.title = '移除 ' + path;
      b.setAttribute('aria-label', '移除 ' + path);
      $('attachments').append(b);
    }
    $('editor').placeholder = target
      ? '给这个执行者留言…'
      : stopped
        ? '输入要恢复的工作…'
        : fixed
          ? '继续这次对话，或补充新的要求…'
          : '描述你的目标，或输入 @ 选择项目文件…';
    const inFlight = fixed && active(state());
    const hasDraft = Boolean($('editor').value.trim());
    const submit = $('send'), action = inFlight ? 'stop' : 'send';
    if (action !== submit.dataset.action) {
      submit.dataset.action = action;
      submit.replaceChildren(icon(inFlight ? 'stop' : 'up'));
    }
    const noteOnly = target && !active(target.state);
    const caption = inFlight ? '停止工单' : stopped ? '恢复并发送' : '发送';
    submit.title = caption;
    submit.setAttribute('aria-label', caption);
    submit.disabled = sending || stopping || (!inFlight && (!hasDraft || noteOnly || (fixed && !t)));
    const append = $('append');
    append.hidden = !inFlight || !hasDraft;
    append.textContent = target ? '定向留言' : '追加';
    append.title = target ? '向选中的任务发送留言' : '向主控追加指令';
    append.disabled = sending || stopping || noteOnly;
    $('submit-hint').textContent = noteOnly ? '任务已停止 · 留言草稿已保留' :
      'Enter ' + (inFlight ? target ? '定向留言' : '追加指令' : '发送') + ' · Shift+Enter 换行';
  }
  function setTarget(job) {
    saveDraft();
    target = job;
    restoreDraft();
    $('editor').focus();
  }
  function addEntry(key, node) {
    view.entries.set(key, node);
    $('feed').append(node);
    if (view.entries.size > 120) {
      const oldest = view.entries.keys().next().value;
      view.entries.get(oldest).remove();
      view.entries.delete(oldest);
    }
    return node;
  }
  function card(key, type, label, body, ts, error = false) {
    const node = view.entries.get(key) || addEntry(key, el('article'));
    node.className = 'entry ' + type + (error ? ' error' : '');
    node.replaceChildren();
    if (type === 'activity') {
      node.append(icon(error ? 'shield' : 'activity'), el('span', '', String(body || '').slice(0, 2200)));
      return;
    }
    const head = el('div', 'entry-head'),
      sp = el('span', 'speaker');
    if (type === 'assistant') {
      const img = el('img');
      img.src = 'mark.svg';
      img.alt = '';
      sp.append(img);
    }
    sp.append(document.createTextNode(label));
    head.append(sp, el('time', '', stamp(ts)));
    node.append(head, textView(body));
  }
  function ingest(events) {
    const t = ticket();
    if (!t) return;
    const ledger = $('scroll'),
      stick = ledger.scrollHeight - ledger.scrollTop - ledger.clientHeight < 90;
    for (const e of events) {
      if (e.seq <= view.cursor) continue;
      const p = e.payload ?? '',
        job = snapshot.jobs?.find((j) => j.id === e.job),
        label = agents[job?.agent] || job?.agent || '执行者';
      const key = 'event-' + e.seq;
      if (eventLabels[e.kind] && e.job && e.turn && Number.isFinite(e.ts)) {
        view.latestEvent = { job: e.job, turn: e.turn, label: eventLabels[e.kind], ts: e.ts };
      }
      switch (e.kind) {
        case 'activity': {
          view.latestEvent = { job: e.job, turn: e.turn, label: StudioActivity.title(e), ts: e.ts, agent: e.agent || p.agent };
          if (p.category === 'report') {
            addEntry(key, StudioActivity.report(e, snapshot.jobs));
            break;
          }
          let group = view.progressGroup;
          if (!group || group.count >= 24 || group.element !== $('feed').lastElementChild) {
            group = StudioActivity.group();
            addEntry(key, group.element);
            view.progressGroup = group;
          }
          group.add(e, snapshot.jobs);
          break;
        }
        case 'user':
        case 'user.follow':
          if (e.kind === 'user' && view.goal) break;
          card(key, 'user', '你', p, e.ts);
          if (e.kind === 'user') view.goal = true;
          break;
        case 'delta': {
          if (typeof p !== 'string' || job?.parent) break;
          const item = view.streams.get(e.job) || { key, body: '' };
          item.body = (item.body + p).slice(-60000);
          view.streams.set(e.job, item);
          break;
        }
        case 'reply': {
          const item = view.streams.get(e.job),
            body = cleanReply(p.text);
          if (body)
            card(item?.key || key, 'assistant', label + (job?.parent ? ' · 协作回报' : ''), body, e.ts);
          else if (item) {
            view.entries.get(item.key)?.remove();
            view.entries.delete(item.key);
          }
          view.streams.delete(e.job);
          if (!job?.parent) view.rootReply = true;
          break;
        }
        case 'send':
          card(
            key,
            'activity',
            '',
            label + ' → ' + (agents[p.to] || p.to) + '　' + compact(p.role || p.body, 140),
            e.ts
          );
          break;
        case 'note':
        case 'note.rejected':
          card(
            key,
            'activity',
            '',
            '留言 → 任务 #' + p.job + '　' + compact(p.body, 200),
            e.ts,
            e.kind === 'note.rejected'
          );
          break;
        case 'failed':
        case 'interrupted':
        case 'draining':
        case 'cancelled':
          card(
            key,
            'activity',
            '',
            label + ' · ' + (states[e.kind] || e.kind) + '　' + (typeof p === 'string' ? p : ''),
            e.ts,
            true
          );
          view.streams.delete(e.job);
          break;
        case 'approval.request':
          card(
            key,
            'activity',
            '',
            '待审批 · 任务 #' + e.job + '　' + compact(p.action?.tool || '请求授权', 140),
            e.ts
          );
          break;
        case 'mcp.result': {
          const name = snapshot.extensions?.servers?.find((s) => s.id === p.server)?.name;
          card(
            key,
            'activity',
            '',
            '工具调用' +
              (p.state === 'done' ? '完成' : '失败') +
              ' · ' +
              [name, p.tool].filter(Boolean).join(' / '),
            e.ts,
            p.state !== 'done'
          );
          break;
        }
        case 'approval.decision':
          card(
            key,
            'activity',
            '',
            '审批 #' + p.approval + (p.decision === 'allow' ? ' 已允许' : ' 已拒绝'),
            e.ts
          );
          break;
        case 'auto.learn':
        case 'auto.goal':
        case 'auto.next':
        case 'auto.discovered':
        case 'auto.milestone':
        case 'auto.exhausted':
          card(
            key,
            'activity',
            '',
            {
              'auto.learn': '经验',
              'auto.goal': '目标',
              'auto.next': '下一步',
              'auto.discovered': '发现后续工作',
              'auto.milestone': '阶段回报',
              'auto.exhausted': '穷尽说明'
            }[e.kind] +
              ' · ' +
              (p.lesson || p.objective || p.body || p.summary || ''),
            e.ts
          );
          break;
        case 'auto.backoff':
        case 'auto.retry':
          card(key, 'activity', '', '自动重试 · ' + p.reason, e.ts);
          break;
      }
    }
    if (!view.goal) {
      card('initial', 'user', '你', t.goal, t.created);
      $('feed').prepend(view.entries.get('initial'));
      view.goal = true;
    }
    for (const [id, item] of view.streams) {
      const body = cleanReply(item.body);
      if (body && events.length)
        card(item.key, 'assistant', agents[snapshot.jobs.find((j) => j.id === id)?.agent] || '主控', body, 0);
    }
    const root = rootJob();
    if (!view.rootReply && root?.result && ['done', 'partial'].includes(root.state)) {
      card(
        'last-result',
        'assistant',
        (agents[root.agent] || root.agent) + ' · 最近回报',
        cleanReply(root.result),
        0
      );
      view.rootReply = true;
    }
    if (stick) ledger.scrollTop = ledger.scrollHeight;
  }
  async function refresh() {
    if (current == null) return;
    const targetView = view,
      id = current;
    if (targetView.request) return targetView.request;
    const args = targetView.cursor ? { ticket: id, after: targetView.cursor } : { ticket: id };
    const pending = (async () => {
      const data = await invoke('snapshot', args);
      if (view !== targetView || current !== id) return;
      snapshot = data;
      const recipient = target && data.jobs.find((j) => j.id === target.id);
      if (recipient && !active(recipient.state)) {
        if ($('editor').value.trim()) target = recipient;
        else setTarget(null);
      }
      ingest(data.events || []);
      targetView.cursor = Math.max(targetView.cursor, Number(data.cursor) || 0);
      renderHeader();
      renderComposer();
      StudioPanels.draw();
      connected(true);
    })();
    targetView.request = pending;
    try {
      return await pending;
    } finally {
      if (targetView.request === pending) targetView.request = null;
    }
  }
  async function chooseProject() {
    if (current != null) {
      sessionSettings();
      return;
    }
    const dir = await invoke('chooseProject');
    if (dir && current == null && dir !== project) {
      project = dir;
      attachments = [];
      profile = 'readonly';
      closeFiles();
      renderComposer();
      notice('');
    }
  }
  async function changeProfile() {
    if (current != null) return;
    const old = profile,
      next = $('profile').value;
    if (next === old) return;
    if (next === 'auto' || next === 'full') {
      $('profile').value = old;
      const text =
        '参与者可用当前系统用户权限读取凭据、修改或删除项目外文件、运行命令并对外发送数据。' +
        (next === 'auto'
          ? '自动模式不设累计轮数上限，会在阶段完成后继续搜寻和调用模型，可能反复产生费用。'
          : '本次会话不会逐项询问审批。') +
        '仅在信任的项目和环境中授予此权限。软件按原样提供，在适用法律允许的范围内不提供担保。';
      if (!(await ask('确认高风险授权 · ' + profiles[next], text, '了解风险并启用')) || current != null)
        return;
    }
    profile = next;
    renderComposer();
  }
  async function stop() {
    if (!ticket() || !active(state()) || stopping || sending) return;
    const id = current;
    stopping = true;
    renderComposer();
    try {
      await invoke('cancel', { ticket: id });
      if (id !== current) return;
      if (target) target = { ...target, state: 'cancelled' };
      await refresh();
      notice('已停止此工单的后续调度。');
      await history();
    } finally { stopping = false; renderComposer(); }
  }
  async function send() {
    const editor = $('editor'),
      body = editor.value,
      id = current,
      v = view,
      job = target?.id;
    if (sending || stopping || !body.trim() || (id !== null && !ticket()) || (target && !active(target.state))) return;
    const resume = ticket()?.permission_profile === 'auto' && state() === 'cancelled' && !job;
    if (
      resume &&
      !(await ask(
        '恢复持续自动运行',
        '将以这条新指令恢复完全访问和持续模型调用，可能重复产生费用。',
        '恢复并发送'
      ))
    )
      return;
    sending = true;
    renderComposer();
    closeFiles();
    try {
      let next = id;
      if (id === null)
        next = await invoke('create', {
          goal: body,
          cwd: project,
          lead,
          permission_profile: profile,
          selections: draftRoutes ?? defaults,
          allow_lead_routing: allowRouting,
          files: [...attachments],
          ...(draftExtensions ? { extensions: draftExtensions } : {})
        });
      else if (job) await invoke('note', { ticket: id, job, body });
      else await invoke('follow', { ticket: id, body, resume: Boolean(resume) });
      if (v === view) {
        if (editor.value === body) editor.value = '';
        attachments = [];
        drafts.delete(draftKey());
        if (id === null) {
          await openChat(next, false);
        } else {
          if (target) setTarget(null);
          await refresh();
        }
        notice(job ? '定向留言已提交。' : '');
      }
      await history();
    } catch (e) {
      report(e);
    } finally {
      sending = false;
      renderComposer();
    }
  }
  function modelSettings(scope) {
    const id = current;
    const readOnly =
      scope === 'session' &&
      id !== null &&
      (snapshot?.jobs || []).some((j) => active(j.state) || j.state === 'interrupted');
    const chosen =
      scope === 'global'
        ? defaults
        : id === null
          ? (draftRoutes ?? defaults)
          : routeMap(ticket()?.agent_routes);
    ModelSettings.open({
      title: scope === 'global' ? '模型与执行端 · 新会话默认' : '本次会话的模型与执行端',
      routes: chosen,
      initialAgent: ticket()?.lead || lead,
      locked: readOnly,
      onSave: async (selections) => {
        if (scope === 'global') {
          await invoke('saveDefaults', { selections });
          defaults = selections;
        } else await saveSessionModels(id, selections);
        renderComposer();
        notice('模型与执行端设置已保存。');
      }
    });
  }
  async function saveSessionModels(id, selections) {
    if (id !== current) throw Error('会话已改变，请重新打开模型选择。');
    if (id === null) draftRoutes = selections;
    else {
      await invoke('saveRoutes', { ticket: id, selections });
      await refresh();
    }
  }
  let modelMenuRun = 0;
  function closeModelMenu() {
    ++modelMenuRun;
    $('model-popover').hidden = true;
    $('model-button').setAttribute('aria-expanded', 'false');
  }
  async function modelMenu() {
    const menu = $('model-popover');
    if (!menu.hidden) { closeModelMenu(); return; }
    closeFiles();
    const run = ++modelMenuRun, id = current, agent = ticket()?.lead || lead;
    const routes = structuredClone(id === null ? draftRoutes ?? defaults : routeMap(ticket()?.agent_routes));
    const pinned = routes[agent], bound = rootJob()?.route;
    const locked = id !== null && (snapshot?.jobs || []).some(j => active(j.state) || j.state === 'interrupted');
    menu.hidden = false;
    $('model-button').setAttribute('aria-expanded','true');
    const heading = el('strong','',agents[agent] + ' · 主控模型');
    const status = el('div','small-note',locked ? '当前工单的模型已锁定。' : '正在读取模型…');
    const list = el('div','model-choices');
    const more = button('更多模型与执行端…',()=>{closeModelMenu();modelSettings('session');},'quiet');
    menu.replaceChildren(heading,status,list,more);
    more.focus();
    async function choose(route) {
      if (locked || run !== modelMenuRun) return;
      for (const b of list.children) b.disabled = true;
      if (route) routes[agent] = route; else delete routes[agent];
      try {
        await saveSessionModels(id,routes);
        if (run !== modelMenuRun) return;
        closeModelMenu();renderComposer();$('model-button').focus();
      } catch (e) {
        if (run === modelMenuRun) { status.textContent = e.message; for (const b of list.children) b.disabled = b.dataset.unavailable === 'true'; }
      }
    }
    function option(label,route,selected,disabled=false) {
      const b=button(label,()=>choose(route),'model-choice',selected?'check':null);
      b.title=label;b.disabled=locked || disabled;b.dataset.unavailable=String(disabled);
      b.setAttribute('aria-pressed',String(selected));list.append(b);
    }
    try {
      const c=await invoke('catalog',{agent});
      if (run !== modelMenuRun) return;
      const route=bound || pinned || c.default || {};
      const provider=c.providers?.find(p=>p.id===route.provider);
      const mode=c.modes?.find(m=>m.id===route.mode);
      status.textContent=[provider?.label || route.provider, locked?'执行中或已中断，当前配置只读':''].filter(Boolean).join(' · ');
      option('跟随默认',null,!pinned && !bound);
      for (const m of provider?.models || []) {
        option(m.label || m.id,{mode:route.mode,provider:route.provider,model:m.id},
          m.id===route.model && !!(pinned || bound),
          !mode || mode.available===false || provider.available===false || m.available===false ||
          (mode.providers && !mode.providers.includes(route.provider)));
      }
      if (route.model && !provider?.models?.some(m=>m.id===route.model))
        option(route.model+' · 当前不可用',null,true,true);
      (list.querySelector('[aria-pressed=true]:enabled') || list.querySelector('button:enabled') || more).focus();
    } catch (e) { if (run===modelMenuRun) status.textContent=e.message; }
  }
  function extensionSettings(scope) {
    const fixed = scope !== 'global' && current !== null;
    ExtensionSettings.open({
      scope: fixed ? 'ticket' : scope,
      cwd: scope === 'global' ? '' : cwd(),
      locked: fixed,
      saved: fixed
        ? snapshot?.extensions || { skills: [], servers: [] }
        : scope === 'global'
          ? null
          : draftExtensions,
      onSave: async (chosen) => {
        if (scope === 'global') await invoke('extensionDefaults', chosen);
        else draftExtensions = chosen;
        notice('MCP 与技能授权已保存。');
      }
    });
  }
  function sessionSettings() {
    const t = ticket(),
      fixed = current !== null;
    if (fixed && !t) return;
    const d = dialog('会话设置'),
      select = el('select');
    select.disabled = fixed;
    for (const [id, name] of Object.entries(agents)) opt(select, id, name);
    select.value = t?.lead || lead;
    d.body.append(
      field('主控', select),
      el('p', 'small-note', '项目 · ' + (cwd() || '独立临时目录')),
      el('p', 'small-note', '权限 · ' + (profiles[t?.permission_profile || profile] || '沿用工单'))
    );
    const cb = el('input');
    cb.type = 'checkbox';
    cb.disabled = fixed;
    cb.checked = fixed ? Boolean(t.allow_lead_routing) : allowRouting;
    const row = el('label', 'check-line');
    row.append(cb, el('span', '', '允许主控在授权范围内选择协作者的模型与子任务权限'));
    d.body.append(row);
    d.body.append(
      el('p', 'small-note', '主控按任务需要选择执行端和协作者人数，同一执行端可派出多名协作者。')
    );
    d.body.append(
      el(
        'p',
        'small-note',
        fixed ? '主控和权限已随工单固定；新建对话可更改。' : '在输入框下方直接选择权限档位。'
      )
    );
    const configActions = el('div', 'head-actions');
    configActions.append(
      button('本次会话的模型与执行端', () => modelSettings('session'), 'quiet', 'sliders'),
      button('本次会话的 MCP 与技能', () => extensionSettings('draft'), 'quiet', 'layers')
    );
    d.body.append(configActions);
    d.foot.append(button('关闭', closeDialog, 'quiet'));
    if (!fixed)
      d.foot.append(
        button(
          '保存',
          () => {
            lead = select.value;
            allowRouting = cb.checked;
            renderComposer();
            closeDialog();
          },
          'primary'
        )
      );
  }
  function closeFiles() {
    ++fileRun;
    clearTimeout(fileTimer);
    mention = null;
    fileRows = [];
    fileIndex = 0;
    $('file-popover').hidden = true;
  }
  function drawFiles() {
    const box = $('file-options');
    box.replaceChildren();
    for (let i = 0; i < fileRows.length; i++) {
      const path = fileRows[i],
        b = button(path, () => addFile(path), '', 'file');
      b.setAttribute('role', 'option');
      b.setAttribute('aria-selected', String(i === fileIndex));
      box.append(b);
    }
    if (!fileRows.length) box.append(el('div', 'empty', '没有匹配的项目文件。'));
  }
  function fetchFiles(q) {
    const id = ++fileRun,
      dir = cwd();
    clearTimeout(fileTimer);
    if (!dir || current !== null) {
      closeFiles();
      return;
    }
    fileRows = [];
    fileIndex = 0;
    $('file-popover').hidden = false;
    $('file-options').replaceChildren(el('p', 'small-note', '正在查找…'));
    fileTimer = setTimeout(async () => {
      try {
        const paths = await invoke('files', { cwd: dir, q });
        if (id !== fileRun) return;
        fileRows = (paths || [])
          .filter((x) => typeof x === 'string' && !attachments.includes(x))
          .slice(0, 60);
        drawFiles();
      } catch (e) {
        if (id === fileRun) {
          closeFiles();
          report(e);
        }
      }
    }, 120);
  }
  function addFile(path) {
    if (!fileRows.includes(path) || current !== null) return;
    if (attachments.length >= 16) {
      notice('单条指令最多附加 16 个文件。', true);
      return;
    }
    const editor = $('editor');
    if (mention) editor.setRangeText('', mention.start, mention.end, 'end');
    attachments.push(path);
    closeFiles();
    renderComposer();
    editor.focus();
  }
  function observeMention() {
    renderComposer();
    if (current !== null || !cwd()) return;
    const ed = $('editor'),
      left = ed.value.slice(0, ed.selectionStart);
    const hit = /(?:^|\s)@([^\s@]*)$/.exec(left);
    if (!hit) {
      if (mention) closeFiles();
      return;
    }
    mention = { start: ed.selectionStart - hit[1].length - 1, end: ed.selectionStart };
    $('file-search').value = hit[1];
    fetchFiles(hit[1]);
  }
  async function openFiles() {
    if (current !== null) {
      notice('可在新会话首条指令中附加项目文件。');
      return;
    }
    if (!project) await chooseProject();
    if (!project) return;
    mention = null;
    $('file-search').value = '';
    fetchFiles('');
    $('file-search').focus();
  }
  function fileKeys(e) {
    if ($('file-popover').hidden) return false;
    if (e.key === 'Escape') {
      closeFiles();
      $('editor').focus();
      return true;
    }
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      if (fileRows.length)
        fileIndex = (fileIndex + (e.key === 'ArrowDown' ? 1 : -1) + fileRows.length) % fileRows.length;
      drawFiles();
      $('file-options').querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
      return true;
    }
    if ((e.key === 'Enter' || e.key === 'Tab') && !e.ctrlKey && fileRows[fileIndex]) {
      addFile(fileRows[fileIndex]);
      return true;
    }
    return false;
  }
  async function exportTicket() {
    if (!ticket()) return;
    const file = await invoke('export', { ticket: current });
    if (file) notice('完整记录已导出至 ' + String(file));
  }
  function more() {
    const d = dialog('会话操作');
    const command = (name, fn, glyph, disabled = false) => {
      const b = button(
        name,
        () => {
          closeDialog();
          Promise.resolve().then(fn).catch(report);
        },
        'command-item',
        glyph
      );
      b.disabled = disabled;
      d.body.append(b);
    };
    command('会话设置', sessionSettings, 'sliders');
    command('本次会话的模型与执行端', () => modelSettings('session'), 'layers');
    command('本次会话的 MCP 与技能', () => extensionSettings('draft'), 'layers');
    command('导出完整事件记录…', exportTicket, 'download', !ticket());
    d.foot.append(button('关闭', closeDialog, 'quiet'));
  }
  function commandItems() {
    return [
      ['新建对话', newChat, 'plus', 'Ctrl N'],
      [
        '搜索会话',
        () => {
          if (window.innerWidth <= 720) $('shell').classList.add('mobile-rail');
          else {
            $('shell').classList.remove('rail-off');
            if (window.innerWidth <= 1050) StudioPanels.toggle(false);
          }
          $('history-search').focus();
        },
        'search',
        ''
      ],
      ['选择项目目录', chooseProject, 'folder', ''],
      ['模型与执行端', () => modelSettings('global'), 'sliders', 'Ctrl Shift M'],
      ['会话设置', sessionSettings, 'sliders', ''],
      ['MCP 与技能', () => extensionSettings('global'), 'layers', ''],
      ['本次会话的 MCP 与技能', () => extensionSettings('draft'), 'layers', ''],
      ['附加项目文件', openFiles, 'attach', '@'],
      ['查看协作任务', () => StudioPanels.select('tasks'), 'chat', ''],
      ['打开项目计划', () => StudioPanels.select('plan'), 'plan', ''],
      ['查看 Git 变更', () => StudioPanels.select('diff'), 'git', ''],
      ...(ticket() ? [['导出完整记录', exportTicket, 'download', '']] : [])
    ];
  }
  function execute(i) {
    const item = commandRows[i];
    if (!item) return;
    $('palette').close();
    Promise.resolve().then(item[1]).catch(report);
  }
  function drawCommands() {
    commandRows = commandItems().filter((r) =>
      r[0].toLowerCase().includes($('command-search').value.toLowerCase())
    );
    commandIndex = Math.max(0, Math.min(commandIndex, commandRows.length - 1));
    const box = $('commands');
    box.replaceChildren();
    commandRows.forEach(([name, fn, glyph, shortcut], i) => {
      const b = button(name, () => execute(i), 'command-item' + (i === commandIndex ? ' active' : ''), glyph);
      if (shortcut) b.append(el('kbd', '', shortcut));
      box.append(b);
    });
    if (!commandRows.length) box.append(el('p', 'empty', '没有匹配的命令。'));
  }
  function palette() {
    if ($('dialog').open) return;
    closeFiles();
    commandIndex = 0;
    $('command-search').value = '';
    if (!$('palette').open) $('palette').showModal();
    drawCommands();
    $('command-search').focus();
  }
  function bind(id, fn) {
    if (observing && id !== 'activity-history') return;
    $(id).addEventListener('click', () => Promise.resolve().then(fn).catch(report));
  }
  for (const [id, title] of Object.entries(profiles)) opt($('profile'), id, title);
  $('profile').onchange = () => void changeProfile().catch(report);
  $('editor').oninput = observeMention;
  $('editor').onkeydown = (e) => {
    if (observing || e.isComposing) return;
    if (fileKeys(e)) {
      e.preventDefault();
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.altKey && e.keyCode !== 229) {
      e.preventDefault();
      void send().catch(report);
    }
  };
  $('file-search').oninput = (e) => fetchFiles(e.target.value);
  $('file-search').onkeydown = (e) => {
    if (!e.isComposing && fileKeys(e)) e.preventDefault();
  };
  document.addEventListener('pointerdown', (e) => {
    if (!$('model-popover').hidden && !$('model-popover').contains(e.target) && !$('model-button').contains(e.target)) closeModelMenu();
    if (
      !$('file-popover').hidden &&
      !$('file-popover').contains(e.target) &&
      !$('attach-button').contains(e.target) &&
      e.target !== $('editor')
    )
      closeFiles();
  });
  $('command-search').oninput = () => {
    commandIndex = 0;
    drawCommands();
  };
  $('command-search').onkeydown = (e) => {
    if (e.isComposing) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (commandRows.length)
        commandIndex =
          (commandIndex + (e.key === 'ArrowDown' ? 1 : -1) + commandRows.length) % commandRows.length;
      drawCommands();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      execute(commandIndex);
    }
  };
  window.addEventListener('keydown', (e) => {
    if (observing || e.isComposing) return;
    if ($('dialog').open || $('palette').open) return;
    if (!$('model-popover').hidden && e.key === 'Escape') {
      closeModelMenu();$('model-button').focus();return;
    }
    if (e.ctrlKey || e.metaKey) {
      const key = e.key.toLowerCase();
      if (key === 'k') {
        e.preventDefault();
        palette();
      } else if (key === 'n') {
        e.preventDefault();
        newChat();
      } else if (key === 'm' && e.shiftKey) {
        e.preventDefault();
        modelSettings('global');
      }
    } else if (e.key === 'Escape') {
      if (!$('file-popover').hidden) closeFiles();
      else if (target) setTarget(null);
      else StudioPanels.toggle(false);
    }
  });
  let searchTimer = 0;
  $('history-search').oninput = () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => history().catch(report), 180);
  };
  for (const b of document.querySelectorAll('[data-template]')) {
    const text = {
      review: '梳理项目的目录、主要入口和测试方式，给出可验证的结论。',
      debug: '定位并修复下面的问题，跑相关测试并说明变更。\n\n复现步骤：',
      plan: '比较下面需求的可行方案、成本与验证步骤，并列出实施计划。\n\n需求：'
    };
    b.onclick = () => {
      $('editor').value = text[b.dataset.template];
      renderComposer();
      $('editor').focus();
    };
  }
  bind('new-chat', newChat);
  bind('toggle-rail', openRail);
  bind('open-models', () => modelSettings('global'));
  bind('open-extensions', () => extensionSettings('global'));
  bind('project-button', chooseProject);
  bind('session-settings', sessionSettings);
  bind('attach-button', openFiles);
  bind('files-close', () => {
    closeFiles();
    $('editor').focus();
  });
  $('model-button').setAttribute('aria-haspopup','dialog');
  $('model-button').setAttribute('aria-expanded','false');
  bind('model-button', modelMenu);
  bind('append', send);
  bind('send', () => active(state()) ? stop() : send());
  bind('more', more);
  bind('activity-history', () => StudioActivity.history(current, snapshot?.jobs || []));
  bind('approval-shortcut', StudioPanels.showApprovals);
  bind('command-button', palette);
  bind('palette-close', () => $('palette').close());
  StudioPanels.init({ snapshot: () => snapshot, ticket, cwd, refresh, target: setTarget, observing });
  document.addEventListener('studio:navigate-ticket', e => {
    if (observing) return;
    if (e.detail === null) newChat();
    else if (Number.isSafeInteger(e.detail) && e.detail > 0) void openChat(e.detail).catch(report);
  });
  async function bootEditor() {
    renderHeader();
    renderComposer();
    $('editor').focus();
    const results = await Promise.allSettled([history(), invoke('defaults')]);
    if (results[0].status === 'rejected') {
      connected(false);
      report(results[0].reason);
    }
    if (results[1].status === 'fulfilled') {
      defaults = routeMap(results[1].value);
      renderComposer();
    } else report(results[1].reason);
  }
  async function boot() {
    if (observing) {
      document.body.classList.add('observing');
      $('observer-label').hidden = false;
      $('welcome').hidden = true;
      const id = Number(observeParam);
      if (!Number.isSafeInteger(id) || id < 1) throw Error('观察工单编号无效。');
      document.title = 'Super Agent · 工单 #' + id + ' 实时进展';
      try { await openChat(id, false); } catch (e) { report(e); }
      StudioPanels.select('tasks');
    } else {
      if (typeof Workbench !== 'undefined') Workbench.init();
      await bootEditor();
    }
    let ticking = false,
      ticks = 0;
    setInterval(async () => {
      if (ticking || document.hidden) return;
      ticking = true;
      try {
        await refresh();
        if (!observing && ++ticks % 5 === 0) await history();
      } catch (e) {
        connected(false);
        notice('读取工作台失败：' + String(e.message || e), true);
      } finally {
        ticking = false;
      }
    }, 900);
  }
  void boot().catch(report);
})();
