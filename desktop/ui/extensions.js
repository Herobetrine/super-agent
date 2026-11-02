'use strict';
const ExtensionSettings = (() => {
  const { el, button, field, dialog, closeDialog, invoke, compact } = StudioUI;
  const trust =
    'stdio 会以当前用户权限启动本机程序。HTTP 会连接服务并可能传输任务内容。请只授权可信来源；不要在界面中粘贴密钥。';
  function open({ scope = 'global', cwd = '', saved = null, locked = false, onSave }) {
    const title = scope === 'global' ? 'MCP 与技能' : '本次会话的 MCP 与技能';
    const d = dialog(title),
      notices = el('p', 'small-note');
    notices.setAttribute('role', 'status');
    let data,
      tab = 'skills',
      query = '',
      working = false,
      selection = null,
      follow = scope !== 'global' && !saved;
    const ids = (v) => (v || []).map((x) => (typeof x === 'string' ? x : x.id));
    const normalize = (v) => ({ skills: ids(v?.skills), servers: ids(v?.servers) });
    function problem(e) {
      notices.textContent = e?.message || String(e);
      notices.className = 'callout danger';
    }
    async function reload(reset = false) {
      try {
        data = await invoke('extensions', cwd ? { cwd } : {});
        if (!d.alive()) return;
        if (!selection || reset) selection = normalize(saved || data.defaults);
        if (!locked) {
          selection.servers = selection.servers.filter((id) =>
            data.servers.some((s) => s.id === id && s.enabled && Array.isArray(s.tools))
          );
          selection.skills = selection.skills.filter((id) => data.skills.some((s) => s.id === id));
        }
        render();
      } catch (e) {
        d.body.replaceChildren(
          notices,
          button('重试', () => reload(), 'quiet')
        );
        problem(e);
        d.foot.replaceChildren(button('关闭', closeDialog, 'quiet'));
      }
    }
    function entryRows() {
      const arr = [...(data[tab] || [])];
      if (locked)
        for (const item of saved?.[tab] || []) {
          const id = typeof item === 'string' ? item : item.id;
          if (!arr.some((x) => x.id === id)) arr.push({ id, name: item.name || id, missing: true });
        }
      return arr.filter(
        (x) =>
          !query ||
          [x.name, x.description, x.source].some((s) =>
            String(s || '')
              .toLowerCase()
              .includes(query.toLowerCase())
          )
      );
    }
    function applyList(list) {
      list.replaceChildren();
      const rows = entryRows();
      if (!rows.length)
        list.append(
          el(
            'p',
            'empty',
            query ? '没有匹配的项目。' : tab === 'skills' ? '未发现已安装的技能。' : '尚未配置 MCP 服务。'
          )
        );
      for (const row of rows) {
        const card = el('div', 'ext-row'),
          line = el('label', 'check-line'),
          check = el('input');
        check.type = 'checkbox';
        const selected = selection[tab].includes(row.id),
          inactive = tab === 'servers' && !row.enabled;
        const unprobed = tab === 'servers' && !Array.isArray(row.tools);
        check.checked = selected;
        check.disabled = locked || follow || (!selected && (row.missing || inactive || unprobed));
        check.setAttribute('aria-label', '选择 ' + (row.name || row.id));
        check.onchange = () => {
          selection[tab] = check.checked
            ? [...new Set([...selection[tab], row.id])]
            : selection[tab].filter((id) => id !== row.id);
          updateFooter();
        };
        line.append(check, el('strong', '', row.name || row.id));
        if (row.missing || inactive || unprobed)
          line.append(el('span', 'tag', row.missing ? '配置已移除' : inactive ? '服务未启用' : '待探测工具'));
        else if (tab === 'servers') line.append(el('span', 'tag', row.tools.length + ' 个工具'));
        card.append(line);
        if (tab === 'skills') {
          const actions = el('div', 'head-actions');
          actions.append(button('预览技能', () => preview(row), 'quiet', 'file'));
          if (row.imported && scope === 'global')
            actions.append(button('取消登记', () => forget(row), 'quiet'));
          card.append(
            el('div', 'small-note', compact(row.description || '', 220)),
            el('div', 'ext-source', String(row.source || '')),
            actions
          );
        } else {
          card.append(
            el(
              'code',
              'ext-source',
              row.transport === 'http' ? 'HTTP · ' + (row.url || '') : 'stdio · ' + (row.command || '')
            )
          );
          if (scope === 'global') {
            const actions = el('div', 'head-actions');
            actions.append(
              button('编辑', () => edit(row), 'quiet'),
              button('探测工具', () => probe(row), 'quiet', 'activity')
            );
            card.append(actions);
          }
        }
        list.append(card);
      }
    }
    function updateFooter() {
      d.foot.replaceChildren();
      d.foot.append(
        el(
          'span',
          'small-note ext-count',
          '技能 ' + selection.skills.length + ' · MCP ' + selection.servers.length
        ),
        button(locked ? '关闭' : '取消', closeDialog, 'quiet')
      );
      if (!locked) {
        const b = button(
          '保存授权',
          async () => {
            if (working) return;
            working = true;
            b.disabled = true;
            try {
              await onSave(follow ? null : structuredClone(selection));
              if (d.alive()) closeDialog();
            } catch (e) {
              problem(e);
            } finally {
              working = false;
              b.disabled = false;
            }
          },
          'primary'
        );
        d.foot.append(b);
      }
    }
    function render() {
      d.body.replaceChildren();
      const tabs = el('div', 'model-tabs');
      for (const [key, title] of [
        ['skills', '技能'],
        ['servers', 'MCP 服务']
      ]) {
        const b = button(
          title + ' ' + (data[key]?.length || 0),
          () => {
            tab = key;
            query = '';
            render();
          },
          key === tab ? 'active' : ''
        );
        tabs.append(b);
      }
      d.body.append(tabs);
      if (scope === 'draft' && !locked) {
        const toggle = el('label', 'check-line'),
          cb = el('input');
        cb.type = 'checkbox';
        cb.checked = follow;
        cb.onchange = () => {
          follow = cb.checked;
          if (follow) selection = normalize(data.defaults);
          render();
        };
        toggle.append(cb, el('span', '', '跟随全局默认授权'));
        d.body.append(toggle);
      }
      d.body.append(
        el(
          'p',
          'small-note',
          locked
            ? '此会话的授权已绑定；新建会话可重新选择。'
            : scope === 'global'
              ? '勾选要默认授予新会话的项目，保存后生效。'
              : '取消跟随后，可单独设置当前会话的授权。'
        )
      );
      if (tab === 'servers')
        d.body.append(
          el('p', 'callout', trust),
          el('p', 'small-note', '启用服务并探测工具后即可勾选。只读权限下，工单不能调用 MCP 工具。')
        );
      const toolbar = el('div', 'ext-tools'),
        filter = el('input', 'field');
      filter.type = 'search';
      filter.value = query;
      filter.placeholder = tab === 'skills' ? '搜索技能或来源' : '搜索 MCP 服务';
      filter.setAttribute('aria-label', filter.placeholder);
      const list = el('div', 'ext-list');
      filter.oninput = () => {
        query = filter.value;
        applyList(list);
      };
      toolbar.append(filter);
      if (scope === 'global') {
        if (tab === 'servers') toolbar.append(button('添加服务', () => edit(null), 'quiet', 'plus'));
        else
          toolbar.append(
            button(
              '导入文件夹',
              async (e) => {
                const b = e.currentTarget;
                b.disabled = true;
                try {
                  const id = await invoke('importSkill');
                  if (id && d.alive()) {
                    notices.className = 'small-note';
                    notices.textContent = '已登记本机技能，请勾选后保存授权。';
                    await reload();
                  }
                } catch (err) {
                  problem(err);
                } finally {
                  b.disabled = false;
                }
              },
              'quiet',
              'folder'
            )
          );
      }
      d.body.append(toolbar, list, notices);
      applyList(list);
      updateFooter();
    }
    function edit(row) {
      const name = el('input'),
        command = el('input'),
        args = el('textarea', 'field ext-args'),
        transport = el('select');
      for (const [id, label] of [
        ['stdio', '本机程序 · stdio'],
        ['http', '远程服务 · Streamable HTTP']
      ]) {
        const o = el('option', '', label);
        o.value = id;
        transport.append(o);
      }
      transport.value = row?.transport === 'http' ? 'http' : 'stdio';
      const endpoint = el('input'),
        tokenEnv = el('input');
      endpoint.value = row?.url || '';
      endpoint.placeholder = 'https://mcp.example.org/mcp';
      tokenEnv.value = row?.tokenEnv || '';
      tokenEnv.placeholder = '可选，例如 MCP_SERVICE_TOKEN';
      name.value = row?.name || '';
      command.value = row?.command || '';
      args.value = JSON.stringify(row?.args || [], null, 2);
      const enabled = el('input');
      enabled.type = 'checkbox';
      enabled.checked = Boolean(row?.enabled);
      const allowed = el('label', 'check-line');
      allowed.append(enabled, el('span', '', '启用服务，允许将它授予会话'));
      const trustCheck = el('input');
      trustCheck.type = 'checkbox';
      const consent = el('label', 'check-line');
      consent.append(trustCheck, el('span', '', '我信任上述程序或服务来源'));
      const localFields = el('div'),
        remoteFields = el('div');
      localFields.append(
        field('可执行程序（单个程序名或路径）', command),
        field('启动参数（JSON 字符串数组）', args)
      );
      remoteFields.append(
        field('MCP 服务地址', endpoint),
        field('令牌环境变量名', tokenEnv),
        el(
          'p',
          'small-note',
          '仅允许 HTTPS 或回环 HTTP。需要令牌时，填入以 MCP_ 开头的环境变量名；不要在 URL 或表单内填写令牌值。'
        )
      );
      function transportFields() {
        localFields.hidden = transport.value === 'http';
        remoteFields.hidden = !localFields.hidden;
        trustCheck.checked = false;
      }
      transport.onchange = transportFields;
      transportFields();
      d.body.replaceChildren(
        el('h3', '', row ? '编辑 MCP 服务' : '添加 MCP 服务'),
        el('p', 'callout danger', trust),
        field('服务名称', name),
        field('传输方式', transport),
        localFields,
        remoteFields,
        allowed,
        consent,
        notices
      );
      notices.textContent = '';
      d.foot.replaceChildren(button('返回列表', render, 'quiet'));
      if (row) d.foot.append(button('移除服务', () => remove(row), 'danger compact'));
      const submit = button(
        '保存服务',
        async () => {
          if (working) return;
          try {
            if (!name.value.trim()) throw Error('填写服务名称。');
            const record = { name: name.value.trim(), enabled: enabled.checked, transport: transport.value };
            if (transport.value === 'http') {
              let url;
              try {
                url = new URL(endpoint.value.trim());
              } catch {
                throw Error('请输入完整的服务地址。');
              }
              const loopback = ['localhost', '[::1]', '127.0.0.1'].includes(url.hostname);
              if (
                url.username ||
                url.password ||
                url.search ||
                url.hash ||
                !(url.protocol === 'https:' || (loopback && url.protocol === 'http:'))
              )
                throw Error('地址须为 HTTPS 或回环 HTTP，且不能携带账号、密码、查询参数或片段。');
              if (tokenEnv.value.trim() && !/^MCP_[A-Z0-9_]{1,96}$/.test(tokenEnv.value.trim()))
                throw Error('变量名须以 MCP_ 开头，后接大写字母、数字或下划线。');
              record.url = url.href;
              record.tokenEnv = tokenEnv.value.trim();
            } else {
              if (!command.value.trim()) throw Error('填写可执行程序。');
              let values;
              try {
                values = JSON.parse(args.value);
              } catch {
                throw Error('启动参数必须是 JSON 数组，例如 ["--stdio"]。');
              }
              if (!Array.isArray(values) || values.some((x) => typeof x !== 'string'))
                throw Error('每个启动参数都必须是字符串。');
              record.command = command.value.trim();
              record.args = values;
            }
            if (enabled.checked && !trustCheck.checked) throw Error('启用前请确认你信任此来源。');
            working = true;
            submit.disabled = true;
            if (row) record.id = row.id;
            await invoke('mcpSave', record);
            if (d.alive()) {
              notices.className = 'small-note';
              notices.textContent = '已保存服务。授权前请先启用服务并探测工具。';
              await reload();
            }
          } catch (e) {
            problem(e);
          } finally {
            working = false;
            submit.disabled = false;
          }
        },
        'primary'
      );
      d.foot.append(submit);
      name.focus();
    }
    function remove(row) {
      d.body.replaceChildren(el('p', 'callout danger', '确定从本机配置移除 ' + row.name + '？'), notices);
      d.foot.replaceChildren(
        button('取消', render, 'quiet'),
        button(
          '确认移除',
          async (e) => {
            const b = e.currentTarget;
            b.disabled = true;
            try {
              await invoke('mcpRemove', { id: row.id });
              selection.servers = selection.servers.filter((id) => id !== row.id);
              if (d.alive()) await reload();
            } catch (err) {
              problem(err);
            } finally {
              b.disabled = false;
            }
          },
          'danger compact'
        )
      );
    }
    function probe(row) {
      const results = el('div', 'ext-list');
      d.body.replaceChildren(
        el('h3', '', '探测 ' + row.name),
        el('p', 'callout danger', trust),
        el(
          'p',
          'small-note',
          row.transport === 'http'
            ? '点击按钮将实际连接此服务地址并读取工具目录。'
            : '点击按钮将实际启动已保存的命令并读取其工具目录。'
        ),
        el(
          'pre',
          'raw',
          row.transport === 'http'
            ? [row.url, row.tokenEnv ? '令牌环境变量：' + row.tokenEnv : ''].filter(Boolean).join('\n')
            : JSON.stringify([row.command, ...(row.args || [])], null, 2)
        ),
        results
      );
      const run = button(
        '连接并读取工具',
        async () => {
          run.disabled = true;
          results.replaceChildren(el('p', 'small-note', '正在连接服务并读取工具目录…'));
          try {
            const tools = await invoke('mcpProbe', { id: row.id });
            results.replaceChildren(el('strong', '', '发现 ' + tools.length + ' 个工具'));
            for (const t of tools) {
              const item = el('div', 'ext-row');
              item.append(el('strong', '', t.name), el('p', 'small-note', compact(t.description, 200)));
              results.append(item);
            }
          } catch (e) {
            results.replaceChildren(el('p', 'callout danger', e.message));
          } finally {
            run.disabled = false;
          }
        },
        'danger compact'
      );
      d.foot.replaceChildren(
        button('返回列表', () => reload(), 'quiet'),
        run
      );
    }
    async function preview(row) {
      const view = el('pre', 'raw skill-preview', '正在读取技能说明…');
      d.body.replaceChildren(el('h3', '', row.name), view);
      d.foot.replaceChildren(button('返回列表', render, 'quiet'));
      try {
        const p = await invoke('skillPreview', { id: row.id, ...(cwd ? { cwd } : {}) });
        view.textContent = String(p.body || '').slice(0, 120000);
      } catch (e) {
        view.textContent = e.message;
      }
    }
    function forget(row) {
      d.body.replaceChildren(el('p', 'callout', '取消登记 ' + row.name + '？原有技能文件会保留。'), notices);
      d.foot.replaceChildren(
        button('取消', render, 'quiet'),
        button(
          '确认取消登记',
          async (e) => {
            const b = e.currentTarget;
            b.disabled = true;
            try {
              await invoke('forgetSkill', { id: row.id });
              selection.skills = selection.skills.filter((id) => id !== row.id);
              if (d.alive()) await reload();
            } catch (err) {
              problem(err);
            } finally {
              b.disabled = false;
            }
          },
          'primary'
        )
      );
    }
    d.body.append(el('p', 'small-note', '正在读取本机配置…'));
    void reload();
  }
  return { open };
})();
