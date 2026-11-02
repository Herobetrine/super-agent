'use strict';
const { app, BrowserWindow, dialog, ipcMain, shell, session, Menu } = require('electron');
const { fork } = require('node:child_process');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { once } = require('node:events');
const { windowMenu }=require('./window-menu.cjs');
const page = path.join(__dirname, 'ui', 'index.html');
const observeArg=process.argv.find(a=>a.startsWith('--observe='));
const observed=observeArg?Number(observeArg.slice('--observe='.length)):null;
if(observeArg && (!Number.isSafeInteger(observed)||observed<1))throw Error('无效的观察工单');
const origin = pathToFileURL(page).href+(observed?'?observe='+observed:'');
let win,
  child,
  seq = 0,
  quitting = false,
  closed = false;
const pending = new Map();
let readyResolve, readyReject;
const ready = new Promise((a, b) => {
  readyResolve = a;
  readyReject = b;
});
ready.catch(() => {});
const methods = new Set([
  'history',
  'snapshot',
  'activityPage',
  'jobsPage',
  'defaults',
  'saveDefaults',
  'saveRoutes',
  'catalog',
  'catalogRevision',
  'create',
  'follow',
  'cancel',
  'retry',
  'note',
  'decide',
  'files',
  'diff',
  'planAdd',
  'planToggle',
  'extensions',
  'extensionDefaults',
  'mcpSave',
  'mcpRemove',
  'mcpProbe',
  'forgetSkill',
  'skillPreview'
]);
const hosts = new Set([
  'developers.openai.com',
  'openai.com',
  'chatgpt.com',
  'learn.chatgpt.com',
  'get.microsoft.com',
  'apps.microsoft.com',
  'github.com',
  'code.claude.com',
  'claude.com',
  'claude.ai',
  'www.deepseek.com',
  'download.deepseek.com',
  'antigravity.google',
  'antigravity.google.com'
]);

function startBroker() {
  child = fork(path.join(__dirname, observed?'../runtime/observer.cjs':'../runtime/broker.cjs'), observed?[String(observed)]:[], {
    env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
    stdio: ['ignore', 'ignore', 'inherit', 'ipc'],
    windowsHide: true
  });
  child.on('message', (m) => {
    if (m.ready) {
      readyResolve();
      return;
    }
    if (m.fatal) {
      readyReject(new Error(m.fatal));
      return;
    }
    if (m.closed) {
      closed = true;
      app.quit();
      return;
    }
    const wait = pending.get(m.id);
    if (!wait) return;
    pending.delete(m.id);
    if (m.error) wait.reject(new Error(m.error));
    else wait.resolve(m.value);
  });
  const fail = (error) => {
    readyReject(error);
    for (const p of pending.values()) p.reject(error);
    pending.clear();
  };
  child.on('error', fail);
  child.once('exit', () => {
    fail(new Error('调度服务已停止，请重开窗口'));
    closed = true;
    if (!quitting && win && !win.isDestroyed()) {
      dialog.showErrorBox('调度服务已停止', '工单记录保存在本机。请重新启动 Super Agent 后检查中断的任务。');
      app.quit();
    }
  });
}
async function call(method, params) {
  await ready;
  if (!child?.connected || quitting) throw new Error('调度服务未连接');
  const id = ++seq;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    child.send({ id, method, params }, (err) => {
      if (err) {
        pending.delete(id);
        reject(err);
      }
    });
  });
}
function focus() {
  if (win) {
    if (win.isMinimized()) win.restore();
    win.show();
    win.focus();
  }
}
function show() {
  win = new BrowserWindow({
    title: 'Super Agent',
    width: 1360,
    height: 880,
    minWidth: 760,
    minHeight: 570,
    backgroundColor: '#ffffff',
    ...(process.platform==='win32'?{titleBarStyle:'hidden',titleBarOverlay:{height:35,color:'#edf2f7',symbolColor:'#363b42'}}:{}),
    icon: path.join(__dirname, '../assets/super-agent.ico'),
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webviewTag: false
    }
  });
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.webContents.on('will-attach-webview', (e) => e.preventDefault());
  win.once('ready-to-show', () => win.show());
  win.on('closed', () => {
    win = null;
  });
  return win.loadFile(page,observed?{query:{observe:String(observed)}}:undefined);
}
if (process.env.SUPERAGENT_ELECTRON_PROFILE)
  app.setPath('userData', path.resolve(process.env.SUPERAGENT_ELECTRON_PROFILE));
if(observed) app.setPath('userData',path.join(app.getPath('userData'),'observers',String(observed)));
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', focus);
  app
    .whenReady()
    .then(async () => {
      Menu.setApplicationMenu(null);
      session.defaultSession.setPermissionRequestHandler((_wc, _permission, cb) => cb(false));
      session.defaultSession.setPermissionCheckHandler(() => false);
      ipcMain.handle('studio', async (e, method, p = {}) => {
        if (
          !win ||
          e.sender !== win.webContents ||
          e.senderFrame !== e.sender.mainFrame ||
          e.senderFrame.url !== origin
        )
          throw new Error('拒绝来自其他页面的请求');
        if (p === null || typeof p !== 'object' || Array.isArray(p)) throw new Error('参数格式无效');
        if(method==='windowMenu')return windowMenu(win,p,!!observed);
        if(observed){
          if(!['history','defaults','snapshot','activityPage','jobsPage'].includes(method))
            throw new Error('实时进展窗口为只读');
          return call(method,p);
        }
        if (method === 'importSkill') {
          const result = await dialog.showOpenDialog(win, {
            title: '选择含有 SKILL.md 的技能文件夹',
            properties: ['openDirectory']
          });
          return result.canceled ? null : call('registerSkill', { folder: result.filePaths[0] });
        }
        if (method === 'chooseProject') {
          const result = await dialog.showOpenDialog(win, {
            title: '选择项目目录',
            properties: ['openDirectory']
          });
          return result.canceled ? null : call('authorize', { cwd: result.filePaths[0] });
        }
        if (method === 'export') {
          const result = await dialog.showSaveDialog(win, {
            title: '导出完整工单记录（请选择新文件）',
            defaultPath: 'ticket-' + Number(p.ticket) + '.jsonl',
            filters: [{ name: 'JSON Lines', extensions: ['jsonl'] }]
          });
          return result.canceled ? null : call('exportTo', { ticket: p.ticket, filename: result.filePath });
        }
        if (method === 'download') {
          const url = new URL(p.url);
          const choices = await call('catalog', { agent: p.agent });
          if (
            url.protocol !== 'https:' ||
            url.username ||
            url.password ||
            !hosts.has(url.hostname) ||
            !choices.downloads?.some((d) => d.url === p.url)
          )
            throw new Error('非官方安装链接');
          await shell.openExternal(url.href);
          return;
        }
        if (!methods.has(method)) throw new Error('未知操作');
        return call(method, p);
      });
      startBroker();
      await ready;
      await show();
    })
    .catch((e) => {
      quitting = true;
      dialog.showErrorBox('无法启动 Super Agent', e.message);
      app.quit();
    });
  app.on('window-all-closed', () => app.quit());
  app.on('before-quit', (e) => {
    if (closed || !child?.connected) return;
    e.preventDefault();
    if (quitting && child.exitCode !== null) return;
    if (child.shutdownRequested) return;
    quitting = true;
    child.shutdownRequested = true;
    child.send({ close: true });
    // Covers a hung supplier or native extension; pipe closure also retires
    // all Windows processes in the Rust-owned job.
    const timer = setTimeout(() => {
      closed = true;
      child.kill();
      app.quit();
    }, 16000);
    timer.unref();
    once(child, 'exit').then(() => {
      clearTimeout(timer);
      closed = true;
      app.quit();
    });
  });
}
