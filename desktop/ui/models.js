'use strict';
const ModelSettings = (() => {
  const { el, button, opt, field, invoke, dialog, closeDialog, agents } = StudioUI;
  function fill(select, rows, id) {
    select.replaceChildren();
    for (const r of rows) opt(select, r.id, r.label || r.id, r.available === false);
    if (id && !rows.some((r) => r.id === id)) opt(select, id, id + ' · 当前不可用', true);
    if (!select.options.length) opt(select, '', '没有可用选项', true);
    select.value = id || '';
    if (select.selectedIndex < 0) select.selectedIndex = 0;
  }
  function open({ title, routes = {}, locked = false, initialAgent = 'deepseek', onSave }) {
    const chosen = structuredClone(routes),
      catalogs = {},
      statuses = {},
      revisions = {},
      pending = new Set();
    let agent = initialAgent,
      refreshTimer = null,
      saveBusy = false;
    const d = dialog(title, () => clearInterval(refreshTimer));
    const tabs = el('div', 'model-tabs'),
      panel = el('section'),
      warning = el('p', 'small-note');
    d.body.append(tabs, panel, warning);
    const discard = button(locked ? '关闭' : '取消', closeDialog, 'quiet');
    const save = button(
      '保存选择',
      async () => {
        if (saveBusy) return;
        saveBusy = true;
        save.disabled = true;
        try {
          await onSave(chosen);
          if (d.alive()) closeDialog();
        } catch (e) {
          warning.textContent = e.message;
          save.disabled = false;
        } finally {
          saveBusy = false;
        }
      },
      'primary'
    );
    d.foot.append(discard);
    if (!locked) d.foot.append(save);
    for (const [id, name] of Object.entries(agents)) {
      const t = button(
        name,
        () => {
          agent = id;
          render();
          load(id, !catalogs[id]);
        },
        ''
      );
      t.dataset.agent = id;
      tabs.append(t);
    }
    async function load(id, force = false) {
      if (pending.has(id)) return;
      pending.add(id);
      let checkedRevision = false;
      try {
        const revision = await invoke('catalogRevision', { agent: id });
        checkedRevision = true;
        if (!d.alive() || agent !== id || (!force && revision === revisions[id])) return;
        revisions[id] = revision;
        statuses[id] = 'loading';
        validate();
        const data = await invoke('catalog', { agent: id });
        if (!d.alive()) return;
        catalogs[id] = data;
        statuses[id] = 'ready';
        if (id === agent) render();
      } catch (e) {
        if (!checkedRevision) delete revisions[id];
        delete catalogs[id];
        statuses[id] = e.message || '获取失败';
        if (d.alive() && id === agent) render();
      } finally {
        pending.delete(id);
        if (d.alive()) validate();
      }
    }
    function render() {
      for (const t of tabs.children) t.classList.toggle('active', t.dataset.agent === agent);
      panel.replaceChildren();
      const c = catalogs[agent];
      if (!c) {
        panel.append(
          el(
            'p',
            'callout',
            !statuses[agent] || statuses[agent] === 'loading' ? '正在读取本机配置…' : statuses[agent]
          )
        );
        panel.append(button('刷新目录', () => load(agent, true), 'quiet', 'refresh'));
        return;
      }
      const mode = el('select'),
        provider = el('select'),
        model = el('select'),
        detail = el('p', 'small-note');
      mode.setAttribute('aria-label', '执行端');
      provider.setAttribute('aria-label', '供应商');
      model.setAttribute('aria-label', '模型');
      const follow = el('input');
      follow.type = 'checkbox';
      follow.checked = !chosen[agent];
      follow.disabled = locked;
      const toggle = el('label', 'check-line');
      toggle.append(follow, el('span', '', '跟随本机适配器默认配置'));
      panel.append(
        toggle,
        el('p', 'small-note', c.detail || ''),
        field('执行端', mode),
        field('供应商', provider),
        field('模型', model),
        detail
      );
      const links = el('div', 'head-actions');
      panel.append(links);
      for (const l of c.downloads || [])
        links.append(
          button(
            l.label,
            () =>
              invoke('download', { agent, url: l.url }).catch((e) => {
                warning.textContent = e.message;
              }),
            'download-link',
            'download'
          )
        );
      links.append(button('刷新', () => load(agent, true), 'quiet', 'refresh'));
      const active = chosen[agent] || c.default || {};
      function draw(pick, resetProvider = false, resetModel = false) {
        const modes = c.modes || [];
        fill(mode, modes, pick.mode);
        const m = modes.find((x) => x.id === mode.value);
        const providers = (c.providers || []).filter((p) => !m?.providers || m.providers.includes(p.id));
        const provId = resetProvider ? providers.find((p) => p.available !== false)?.id || '' : pick.provider;
        fill(provider, providers, provId);
        const p = providers.find((x) => x.id === provider.value);
        const models = p?.models || [];
        const modelId = resetModel
          ? models.find((x) => x.id === p?.defaultModel && x.available !== false)?.id ||
            models.find((x) => x.available !== false)?.id ||
            ''
          : pick.model;
        fill(model, models, modelId);
        for (const s of [mode, provider, model]) s.disabled = locked || follow.checked;
        detail.textContent = [m?.detail, p?.detail].filter(Boolean).join(' · ');
      }
      function store() {
        if (follow.checked) delete chosen[agent];
        else chosen[agent] = { mode: mode.value, provider: provider.value, model: model.value };
        const ok =
          follow.checked ||
          [mode, provider, model].every(
            (s) => s.value && s.selectedOptions[0] && !s.selectedOptions[0].disabled
          );
        if (!ok) {
          warning.textContent = '这个组合不可用，请更换选项或跟随默认配置。';
        } else
          warning.textContent = locked ? '会话中仍有活动任务，当前配置只读。' : '保存后只使用选中的组合。';
        validate();
      }
      draw(active);
      follow.onchange = () => {
        draw(follow.checked ? c.default : { mode: mode.value, provider: provider.value, model: model.value });
        store();
      };
      mode.onchange = () => {
        draw({ mode: mode.value }, true, true);
        store();
      };
      provider.onchange = () => {
        draw({ mode: mode.value, provider: provider.value }, false, true);
        store();
      };
      model.onchange = store;
      validate();
      if (chosen[agent] && [mode, provider, model].some((s) => s.selectedOptions[0]?.disabled))
        warning.textContent = '已选组合在最新配置中不可用，请重新选择或跟随本机默认配置。';
      else warning.textContent = locked ? '会话中仍有活动任务，当前配置只读。' : '保存后只使用选中的组合。';
    }
    function validate() {
      save.disabled =
        saveBusy ||
        Object.entries(chosen).some(([a, s]) => {
          const c = catalogs[a];
          if (!c || statuses[a] !== 'ready') return !!statuses[a] || a === agent;
          const m = c.modes?.find((x) => x.id === s.mode),
            p = c.providers?.find((x) => x.id === s.provider);
          return (
            !m ||
            m.available === false ||
            !p ||
            p.available === false ||
            (m.providers && !m.providers.includes(p.id)) ||
            !p.models?.some((x) => x.id === s.model && x.available !== false)
          );
        });
    }
    render();
    load(agent, true);
    // Unchanged revisions bypass provider discovery, including for a focused control.
    refreshTimer = setInterval(() => {
      if (d.alive()) load(agent);
    }, 2000);
  }
  return { open };
})();
