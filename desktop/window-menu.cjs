'use strict';
const { Menu, app, dialog, shell } = require('electron');

function windowMenu(win, { section, x, y }, observing) {
  const action = (label, id) => ({ label, id });
  const groups = {
    file: [
      ...(!observing
        ? [
            action('新建对话', 'new-chat'),
            action('选择项目…', 'project-button'),
            action('MCP 与技能…', 'open-extensions'),
            { type: 'separator' }
          ]
        : []),
      { role: 'close', label: '关闭窗口' }
    ],
    edit: [
      { role: 'undo', label: '撤销' },
      { role: 'redo', label: '重做' },
      { type: 'separator' },
      { role: 'cut', label: '剪切' },
      { role: 'copy', label: '复制' },
      { role: 'paste', label: '粘贴' },
      { role: 'selectAll', label: '全选' }
    ],
    view: [
      ...(!observing
        ? [
            action('切换项目侧栏', 'toggle-rail'),
            action('切换项目面板', 'toggle-inspector'),
            action('命令面板…', 'command-button'),
            { type: 'separator' }
          ]
        : []),
      { role: 'zoomIn', label: '放大' },
      { role: 'zoomOut', label: '缩小' },
      { role: 'resetZoom', label: '恢复实际大小' },
      { role: 'togglefullscreen', label: '切换全屏' }
    ],
    help: [
      {
        label: '使用说明',
        click: () => shell.openExternal('https://github.com/Herobetrine/super-agent#readme')
      },
      {
        label: '关于 Super Agent',
        click: () =>
          dialog.showMessageBox(win, {
            type: 'info',
            title: '关于 Super Agent',
            message: 'Super Agent',
            detail: '本机多 Agent 项目工作台\n版本 ' + app.getVersion() + ' · GPL-3.0-only',
            buttons: ['关闭']
          })
      }
    ]
  };
  if (!Object.hasOwn(groups, section)) throw Error('菜单不存在');
  const [width, height] = win.getContentSize();
  return new Promise((resolve) => {
    const template = groups[section].map((item) =>
      item.id ? { label: item.label, click: () => resolve(item.id) } : item
    );
    Menu.buildFromTemplate(template).popup({
      window: win,
      x: Math.min(width, Math.max(0, Math.round(Number(x) || 0))),
      y: Math.min(height, Math.max(0, Math.round(Number(y) || 0))),
      callback: () => resolve(null)
    });
  });
}
module.exports = { windowMenu };
