'use strict';
const StudioUI = (() => {
  const agents = {
    deepseek: 'DeepSeek Harness',
    codex: 'Codex',
    claude: 'Claude Code',
    antigravity: 'Antigravity'
  };
  const states = {
    queued: '等待执行',
    running: '执行中',
    waiting: '等待协作',
    draining: '收集回报',
    done: '已完成',
    failed: '失败',
    partial: '部分完成',
    cancelled: '已停止',
    interrupted: '已中断'
  };
  const profiles = {
    readonly: '只读',
    ask: '寻求审批',
    partial: '部分审批',
    full: '完全访问 · 高危',
    auto: '自动模式 · 高危'
  };
  const icons = {
    chevron: 'm7 10 5 5 5-5',
    plus: 'M12 5v14M5 12h14',
    search: 'M20 20l-5-5M17 10a7 7 0 1 1-14 0 7 7 0 0 1 14 0',
    sliders: 'M3 6h8m4 0h6M11 3v6M3 17h4m4 0h10M7 14v6',
    command:
      'M8 8h8v8H8zM8 8H5a3 3 0 1 1 3-3v3zm8 0V5a3 3 0 1 1 3 3h-3zm0 8h3a3 3 0 1 1-3 3v-3zm-8 0v3a3 3 0 1 1-3-3h3z',
    panel: 'M3 4h18v16H3zM9 4v16',
    layout: 'M3 4h18v16H3zM15 4v16',
    more: 'M5 12h.1m6.9 0h.1m6.9 0h.1',
    shield: 'M12 3l8 3v6c0 5-5 8-8 10-3-2-8-5-8-10V6zM8 12l3 3 5-6',
    stop: 'M6 6h12v12H6z',
    layers: 'M3 8l9-5 9 5-9 5zM3 12l9 5 9-5M3 16l9 5 9-5',
    code: 'M7 7l-5 5 5 5m10-10 5 5-5 5m-3-14-4 18',
    plan: 'M4 3h16v18H4zM8 8h8m-8 4h8m-8 4h5',
    close: 'M6 6l12 12M18 6L6 18',
    folder: 'M3 7V5h6l3 3h9v11H3z',
    attach: 'M9 14l7-7a3 3 0 0 1 4 4L10 21a5 5 0 0 1-7-7L14 3',
    up: 'M12 20V4m-6 6 6-6 6 6',
    forward: 'M4 12h15m-6-6 6 6-6 6',
    activity: 'M2 12h5l3-7 4 14 3-7h5',
    chat: 'M3 4h18v13H9l-6 4z',
    file: 'M5 3h9l5 5v13H5zM14 3v6h5',
    check: 'M4 12l5 5L20 6',
    download: 'M12 3v12m-5-5 5 5 5-5M4 16v5h16v-5',
    refresh: 'M20 7v6h-6M4 17v-6h6M6 7a8 8 0 0 1 13 2m-1 8a8 8 0 0 1-13-2',
    git: 'M6 6v12m0-3c6 0 12-1 12-8M6 3a2 2 0 1 1 0 4 2 2 0 0 1 0-4m0 14a2 2 0 1 1 0 4 2 2 0 0 1 0-4m12-14a2 2 0 1 1 0 4 2 2 0 0 1 0-4'
  };
  const $ = (id) => document.getElementById(id);
  function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = String(text);
    return node;
  }
  function icon(key) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('class', 'glyph');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(svg.namespaceURI, 'path');
    path.setAttribute('d', icons[key] || icons.activity);
    svg.append(path);
    return svg;
  }
  document.querySelectorAll('[data-icon]').forEach((node) => node.replaceWith(icon(node.dataset.icon)));
  function button(text, fn, cls, glyph) {
    const b = el('button', cls);
    b.type = 'button';
    if (glyph) b.append(icon(glyph));
    b.append(el('span', '', text));
    b.addEventListener('click', fn);
    return b;
  }
  function opt(select, id, label, disabled = false) {
    const o = el('option', '', label);
    o.value = id;
    o.disabled = disabled;
    select.append(o);
    return o;
  }
  function notice(msg, error = false) {
    $('message-text').textContent = String(msg || '').slice(0, 600);
    $('message').classList.toggle('error', error);
    $('message').hidden = !msg;
  }
  async function invoke(method, params = {}) {
    if (!window.studio || typeof window.studio.invoke !== 'function')
      throw Error('桌面连接尚未就绪，请通过 Super Agent 桌面程序打开。');
    return window.studio.invoke(method, params);
  }
  function report(err) {
    notice(err instanceof Error ? err.message : String(err), true);
  }
  $('dismiss-message').onclick = () => notice('');
  let serial = 0,
    dismiss = null;
  function closeDialog() {
    const dlg = $('dialog');
    if (dlg.open) dlg.close();
    const cleanup = dismiss;
    dismiss = null;
    ++serial;
    if (cleanup) cleanup();
  }
  function dialog(title, onClose) {
    closeDialog();
    dismiss = onClose || null;
    const head = el('header', 'dialog-head');
    head.append(el('h2', '', title));
    const x = button('', closeDialog, 'icon-button', 'close');
    x.setAttribute('aria-label', '关闭');
    head.append(x);
    const body = el('div', 'dialog-body'),
      foot = el('div', 'dialog-footer');
    $('dialog-content').replaceChildren(head, body, foot);
    $('dialog').showModal();
    const token = serial;
    return { body, foot, alive: () => token === serial && $('dialog').open };
  }
  $('dialog').addEventListener('cancel', (e) => {
    e.preventDefault();
    closeDialog();
  });
  function ask(title, message, accept) {
    return new Promise((resolve) => {
      const d = dialog(title, () => resolve(false));
      d.body.append(el('p', 'callout danger', message));
      const cancel = button('取消', closeDialog, 'quiet');
      d.foot.append(
        cancel,
        button(
          accept,
          () => {
            resolve(true);
            closeDialog();
          },
          'danger'
        )
      );
      cancel.focus();
    });
  }
  function field(label, control) {
    const row = el('label', 'form-row');
    row.append(el('span', '', label), control);
    return row;
  }
  function codeBlock(source, language) {
    const box = el('div', 'codebox'),
      bar = el('div', 'code-head'),
      pre = el('pre', 'codeblock', source);
    const copy = button(
      '复制代码',
      () => {
        const selection = window.getSelection();
        const ranges = Array.from({ length: selection.rangeCount }, (_, i) =>
          selection.getRangeAt(i).cloneRange()
        );
        const range = document.createRange();
        range.selectNodeContents(pre);
        selection.removeAllRanges();
        selection.addRange(range);
        if (document.execCommand('copy')) {
          selection.removeAllRanges();
          for (const r of ranges) selection.addRange(r);
          copy.textContent = '已复制';
          setTimeout(() => {
            copy.textContent = '复制代码';
          }, 1500);
        } else notice('已选中代码，可按 Ctrl+C 复制。');
      },
      'quiet'
    );
    bar.append(el('span', '', language || '代码'), copy);
    box.append(bar, pre);
    return box;
  }
  function textView(text) {
    const view = el('div', 'body-text');
    function prose(part) {
      if (!part) return;
      part.split(/(\x60[^\x60\n]+\x60|\*\*[^*\n]+\*\*)/g).forEach((segment) => {
        if (segment.startsWith('\x60') && segment.endsWith('\x60'))
          view.append(el('code', '', segment.slice(1, -1)));
        else if (segment.startsWith('**') && segment.endsWith('**'))
          view.append(el('strong', '', segment.slice(2, -2)));
        else view.append(document.createTextNode(segment));
      });
    }
    const lines = String(text || '')
      .slice(0, 60000)
      .replace(/\r\n/g, '\n')
      .split('\n');
    const buffer = [];
    let fence = '',
      language = '';
    for (const line of lines) {
      if (fence) {
        const end = line.trim();
        if (end.startsWith(fence) && [...end].every((c) => c === fence[0])) {
          view.append(codeBlock(buffer.join('\n'), language));
          buffer.length = 0;
          fence = '';
        } else buffer.push(line);
      } else {
        const opener = /^ {0,3}(\x60{3,}|~{3,})\s*([^\s]*)\s*$/.exec(line);
        if (opener) {
          prose(buffer.join('\n'));
          buffer.length = 0;
          fence = opener[1];
          language = opener[2];
        } else buffer.push(line);
      }
    }
    if (fence) view.append(codeBlock(buffer.join('\n'), language));
    else prose(buffer.join('\n'));
    if (String(text || '').length > 60000)
      view.append(el('small', 'secondary', '\n该消息已缩短预览，可导出完整记录。'));
    return view;
  }
  const compact = (v, n = 84) => {
    const s = String(v ?? '')
      .trim()
      .replace(/\s+/g, ' ');
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  };
  const basename = (s) =>
    String(s || '')
      .split(/[\\/]/)
      .filter(Boolean)
      .pop() || '选择项目';
  const stamp = (ts) => {
    const date = new Date(Number(ts) * 1000);
    return Number.isNaN(date.valueOf())
      ? ''
      : date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false });
  };
  const cleanReply = (s) => String(s || '').split('\n')
    .filter(l => !/^\s*SA_(SEND|ROLE|NOTE|TOOL|PROGRESS|GOAL|NEXT|FINISH|LEARN|EXHAUSTED)(?:\s|$)/.test(l))
    .join('\n').trim();
  function activityTime(ts) {
    if (!Number.isFinite(ts) || ts <= 0) return '';
    const date = new Date(ts * 1000);
    const day = date.toDateString() === new Date().toDateString() ? '' : date.toLocaleDateString('zh-CN') + ' ';
    return day + date.toLocaleTimeString('zh-CN', { hour12: false });
  }
  return {
    cleanReply,
    activityTime,
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
    compact,
    basename,
    stamp,
    agents,
    states,
    profiles
  };
})();
