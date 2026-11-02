'use strict';

const Workbench = (() => {
  const { $, el, icon, basename, states } = StudioUI;
  const observing = new URLSearchParams(location.search).has('observe');
  const storageKey = 'superagent.closed-projects.v1';
  let collapsed = new Set();
  try {
    const saved = JSON.parse(sessionStorage.getItem(storageKey));
    if (Array.isArray(saved)) collapsed = new Set(saved.filter((key) => typeof key === 'string'));
  } catch {
    /* Storage can be unavailable on a local-file origin. */
  }
  let lastSearch = '',
    searchCollapsed = new Set(),
    summaryKey = '',
    selectPanel;

  function searchHistory() {
    const shell = $('shell');
    shell.classList.remove('rail-off');
    if (window.innerWidth <= 720) shell.classList.add('mobile-rail');
    else if (window.innerWidth <= 1050) StudioPanels.toggle(false);
    $('history-search').focus();
  }

  function init() {
    const nav = $('app-nav');
    nav.hidden = observing;
    if (observing || nav.childElementCount) return;
    const actions = [
      ['new', '新建对话', 'plus', () => $('new-chat').click()],
      ['search', '搜索对话或项目', 'search', searchHistory],
      ['tasks', '查看项目协作', 'layers', () => StudioPanels.select('tasks')],
      ['settings', '模型与执行端', 'sliders', () => $('open-models').click()]
    ];
    for (const [action, title, glyph, run] of actions) {
      const b = el('button', 'wb-nav-button');
      b.type = 'button';
      b.dataset.action = action;
      b.title = title;
      b.setAttribute('aria-label', title);
      b.append(icon(glyph));
      b.onclick = run;
      nav.append(b);
    }
    const brand = el('span', 'wb-monogram', 'S');
    brand.title = 'Super Agent';
    brand.setAttribute('aria-label', 'Super Agent');
    nav.append(brand);

    const summary = el('aside', 'wb-summary');
    summary.id = 'workbench-summary';
    summary.setAttribute('aria-label', '工作区概览');
    summary.hidden = true;
    $('inspector').before(summary);
  }

  function projectKey(cwd) {
    if (!cwd) return '';
    const normalized = cwd.replaceAll('\\', '/').replace(/\/+$/, '') || '/';
    return /^[a-z]:/i.test(normalized) || normalized.startsWith('//') ? normalized.toLowerCase() : normalized;
  }

  function organizeHistory(container, rows) {
    if (observing || !rows.length) return;
    const buttons = new Map(
      [...container.querySelectorAll('button[data-ticket-id]')].map((b) => [b.dataset.ticketId, b])
    );
    const groups = new Map();
    for (const row of rows) {
      const button = buttons.get(String(row.id));
      if (!button) continue;
      const cwd = String(row.cwd || '');
      const key = projectKey(cwd);
      if (!groups.has(key)) {
        const parts = cwd.split(/[\\/]/).filter(Boolean);
        groups.set(key, { key, cwd, parts, name: parts.at(-1) || (cwd ? '/' : '无项目'), buttons: [] });
      }
      button.classList.add('wb-chat');
      const state = states[row.state] || '';
      const meta = button.querySelector('small');
      if (meta) meta.textContent = state;
      groups.get(key).buttons.push(button);
    }
    const query = $('history-search').value.trim();
    if (query !== lastSearch) {
      searchCollapsed = new Set();
      lastSearch = query;
    }
    const closed = query ? searchCollapsed : collapsed;
    const list = [...groups.values()];
    const fragment = document.createDocumentFragment();
    for (const group of list) {
      const section = el('div', 'wb-project');
      const head = el('button', 'wb-project-head');
      head.type = 'button';
      head.title = group.cwd || '无项目';
      // Show the shortest distinct path suffix when two project folders share their name.
      const siblings = list.filter(
        (g) => g !== group && g.cwd && g.name.toLowerCase() === group.name.toLowerCase()
      );
      let depth = 1;
      const suffix = (g, n) => g.parts.slice(-n).join('/');
      while (
        depth < group.parts.length &&
        siblings.some((g) => suffix(g, depth).toLowerCase() === suffix(group, depth).toLowerCase())
      )
        depth++;
      head.append(icon('folder'), el('span', 'wb-project-name', suffix(group, depth) || group.name));
      const arrow = icon('chevron');
      arrow.classList.add('wb-chevron');
      head.append(arrow);
      const chats = el('div', 'wb-project-chats');
      chats.id = 'project-chats-' + group.buttons[0].dataset.ticketId;
      head.setAttribute('aria-controls', chats.id);
      chats.append(...group.buttons);
      const sync = () => {
        const open = !closed.has(group.key);
        head.setAttribute('aria-expanded', String(open));
        for (const b of group.buttons) b.hidden = !open && !b.hasAttribute('aria-current');
      };
      head.onclick = () => {
        if (closed.has(group.key)) closed.delete(group.key);
        else closed.add(group.key);
        if (!query) {
          try {
            sessionStorage.setItem(storageKey, JSON.stringify([...collapsed]));
          } catch {}
        }
        sync();
      };
      sync();
      section.append(head, chats);
      fragment.append(section);
    }
    container.replaceChildren(fragment);
  }

  function update(snapshot, openPanel) {
    if (observing) return;
    const box = $('workbench-summary');
    if (!box) return;
    selectPanel = openPanel;
    box.hidden = !snapshot?.ticket;
    if (box.hidden) {
      summaryKey = '';
      return;
    }
    const { ticket, jobs = [], nativeActivity, totalJobs } = snapshot;
    const peers =
      snapshot.runningPeers ?? jobs.filter((job) => job.parent != null && job.state === 'running').length;
    const internal = nativeActivity?.tasks || [];
    const internalRunning = internal.filter((t) => t.state === 'running').length;
    const platformTotal = Math.max(0, (totalJobs ?? jobs.length) - 1);
    const key = JSON.stringify([
      ticket.id,
      ticket.cwd,
      peers,
      platformTotal,
      jobs.length,
      internal.length,
      internalRunning
    ]);
    if (key === summaryKey) return;
    summaryKey = key;
    const title = el('div', 'wb-summary-title');
    title.title = ticket.cwd || '无项目';
    title.append(icon('folder'), el('span', '', ticket.cwd ? basename(ticket.cwd) : '无项目'));
    const lines = [title];
    const link = (tab, glyph, text, value = '') => {
      const b = el('button', 'wb-summary-link');
      b.type = 'button';
      b.append(icon(glyph), el('span', '', text));
      if (value) b.append(el('small', '', value));
      b.onclick = () => selectPanel?.(tab);
      lines.push(b);
    };
    const partial = snapshot.runningPeers == null && totalJobs > jobs.length;
    const count = partial ? `${platformTotal} 项` : `${peers} / ${platformTotal}`;
    link('tasks', 'layers', '平台协作', count);
    lines.at(-1).title = partial ? '协作任务总数' : '执行中 / 协作任务总数';
    if (internal.length) {
      link('tasks', 'activity', '原生任务', `${internalRunning} / ${internal.length}`);
      lines.at(-1).title = '执行中 / 已记录的原生任务';
    }
    if (ticket.cwd) link('diff', 'git', '工作区变更');
    box.replaceChildren(...lines);
  }

  return { init, organizeHistory, update };
})();
