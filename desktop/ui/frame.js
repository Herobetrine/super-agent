'use strict';
(() => {
  const observed = new URLSearchParams(location.search).has('observe');
  const bar = document.createElement('header');
  bar.className = 'window-frame';
  bar.setAttribute('aria-label', '应用窗口工具栏');
  function make(label, symbol, click) {
    const button = document.createElement('button');
    button.type = 'button';
    button.title = label;
    button.setAttribute('aria-label', label);
    if (symbol) {
      const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
      svg.setAttribute('viewBox', '0 0 20 20');
      svg.setAttribute('aria-hidden', 'true');
      const line = document.createElementNS(svg.namespaceURI, 'path');
      line.setAttribute('d', symbol);
      svg.append(line);
      button.append(svg);
    } else button.textContent = label;
    button.addEventListener('click', () => click(button));
    bar.append(button);
    return button;
  }
  if (!observed) {
    const stack = [null];
    let index = 0;
    const nav = (direction) => {
      const next = index + direction;
      if (next < 0 || next >= stack.length) return;
      index = next;
      refresh();
      document.dispatchEvent(new CustomEvent('studio:navigate-ticket', { detail: stack[index] }));
    };
    const back = make('后退', 'M16 10H4m5-5-5 5 5 5', () => nav(-1));
    const forward = make('前进', 'M4 10h12m-5-5 5 5-5 5', () => nav(1));
    function refresh() {
      back.disabled = index === 0;
      forward.disabled = index === stack.length - 1;
    }
    document.addEventListener('studio:ticket-changed', (e) => {
      if (e.detail !== stack[index]) {
        stack.splice(index + 1);
        stack.push(e.detail);
        index = stack.length - 1;
      }
      refresh();
    });
    refresh();
    make('切换侧栏', 'M3.5 5h13v10h-13zm4 0v10', () => document.getElementById('toggle-rail')?.click());
  }
  for (const [key, label] of [
    ['file', '文件'],
    ['edit', '编辑'],
    ['view', '视图'],
    ['help', '帮助']
  ]) {
    const b = make(label, null, async (button) => {
      const rect = button.getBoundingClientRect();
      const action = await window.studio.invoke('windowMenu', { section: key, x: rect.left, y: rect.bottom });
      if (typeof action === 'string') document.getElementById(action)?.click();
    });
    b.setAttribute('aria-haspopup', 'menu');
  }
  document.body.prepend(bar);
})();
