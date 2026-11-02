'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const http = require('node:http');
const https = require('node:https');
const { DatabaseSync } = require('node:sqlite');

const home = () => os.homedir();
const platformDir = () => path.join(home(), '.local/share/superagent');
function readJSON(file, fallback = {}) {
  try {
    return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(fs.readFileSync(file)));
  } catch (e) {
    if (e.code === 'ENOENT') return fallback;
    throw Error('本地 JSON 配置无法读取：' + path.basename(file));
  }
}
function file(p) {
  try {
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}
function which(name) {
  for (const dir of (process.env.PATH || '').split(path.delimiter)) {
    if (!dir || !path.isAbsolute(dir)) continue;
    const p = path.join(dir, name);
    try {
      if (file(p)) {
        fs.accessSync(p, fs.constants.X_OK);
        return p;
      }
    } catch {}
  }
  return null;
}
function ccLocator() {
  return path.join(
    process.env.APPDATA || path.join(home(), 'AppData/Roaming'),
    'com.ccswitch.desktop/app_paths.json'
  );
}
function ccDatabase(override) {
  const folder = override || path.join(home(), '.cc-switch');
  if (!path.isAbsolute(folder)) throw Error('CC Switch 数据目录不是绝对路径');
  return path.join(folder, 'cc-switch.db');
}
function ccWatchPaths() {
  const locator = ccLocator();
  const override = require('./metadata.cjs').pathField(
    'cc-locator',
    locator,
    () => readJSON(locator).app_config_dir_override
  );
  const paths = [locator];
  if (!override || path.isAbsolute(override)) {
    const db = ccDatabase(override);
    paths.push(db, db + '-wal');
  }
  return paths;
}
function ccRows(...apps) {
  const dbfile = ccDatabase(readJSON(ccLocator()).app_config_dir_override);
  if (!file(dbfile)) return [];
  const db = new DatabaseSync(dbfile, { readOnly: true });
  try {
    db.exec('PRAGMA busy_timeout=2000');
    const cols = db
      .prepare('PRAGMA table_info(providers)')
      .all()
      .map((c) => c.name);
    const optional = ['meta', 'category'].map((c) => (cols.includes(c) ? c : 'NULL AS ' + c));
    return db
      .prepare(
        'SELECT app_type,id,name,settings_config,is_current,' +
          optional.join(',') +
          ' FROM providers WHERE app_type IN (' +
          apps.map(() => '?').join(',') +
          ') ORDER BY app_type,name'
      )
      .all(...apps);
  } finally {
    db.close();
  }
}
function fingerprint(prefix, parts) {
  const ascii = parts.map((v) =>
    JSON.stringify(v).replace(
      /[\u007f-\uffff]/g,
      (c) => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0')
    )
  );
  return (
    prefix +
    crypto
      .createHash('sha256')
      .update('[' + ascii.join(', ') + ']')
      .digest('hex')
      .slice(0, 24)
  );
}
const localHost = (host) => ['127.0.0.1', '[::1]', 'localhost'].includes(host);
function endpoint(base) {
  let u;
  try {
    u = new URL(base);
  } catch {
    throw Error('无效的供应商端点');
  }
  if (
    u.username ||
    u.password ||
    u.search ||
    u.hash ||
    !(u.protocol === 'https:' || (u.protocol === 'http:' && localHost(u.hostname)))
  )
    throw Error('不支持的供应商端点');
  return u;
}
async function getJSON(url, headers, signal) {
  const timeout = AbortSignal.timeout(3500);
  const target = new URL(url),
    client = target.protocol === 'https:' ? https : http;
  // Direct connection to the already-validated origin; no global fetch
  // dispatcher, environment proxy or HTTP redirect may carry its credentials.
  const res = await new Promise((resolve, reject) => {
    client
      .get(
        target,
        { headers, agent: false, signal: signal ? AbortSignal.any([signal, timeout]) : timeout },
        resolve
      )
      .once('error', reject);
  });
  if (res.statusCode < 200 || res.statusCode >= 300) {
    res.destroy();
    throw Error('目录 HTTP ' + res.statusCode);
  }
  const data = [];
  let size = 0;
  for await (const chunk of res) {
    size += chunk.length;
    if (size > 1024 * 1024) throw Error('目录超过 1 MiB');
    data.push(chunk);
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(data)));
}
async function killTree(child) {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  if (process.platform === 'win32') {
    await new Promise((resolve) => {
      const killer = spawn(
        path.join(process.env.SystemRoot || 'C:\\Windows', 'System32/taskkill.exe'),
        ['/PID', String(child.pid), '/T', '/F'],
        { windowsHide: true, stdio: 'ignore' }
      );
      killer.once('error', resolve);
      killer.once('close', resolve);
    });
  } else {
    try {
      process.kill(-child.pid, 'SIGKILL');
    } catch {}
  }
  if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
}
function start(exe, args, { cwd, env = process.env, signal } = {}) {
  if (!exe) throw Error('本机未安装所选原生执行端');
  signal?.throwIfAborted();
  const child = spawn(exe, args, {
    cwd,
    env,
    windowsHide: true,
    shell: false,
    detached: process.platform !== 'win32',
    stdio: ['pipe', 'pipe', 'pipe']
  });
  child.stdin.on('error', () => {}); // Broken pipe is surfaced through the terminal/exit contract.
  child.stderr.resume(); // CLI diagnostics can include transport configuration. Never export blindly.
  let kill;
  const stop = () => (kill ||= killTree(child));
  const onAbort = () => {
    void stop();
  };
  signal?.addEventListener('abort', onAbort, { once: true });
  const finished = new Promise((resolve) => {
    child.once('error', () => resolve(-1));
    child.once('close', (code) => resolve(code ?? -1));
  }).finally(() => signal?.removeEventListener('abort', onAbort));
  if (signal?.aborted) onAbort();
  return { child, finished, stop };
}
async function capture(exe, args, { input = '', timeout = 10000, signal, ...options } = {}) {
  const deadline = AbortSignal.timeout(timeout);
  const active = signal ? AbortSignal.any([deadline, signal]) : deadline;
  const p = start(exe, args, { ...options, signal: active });
  p.child.stdin.end(input);
  const chunks = [];
  let size = 0;
  try {
    for await (const chunk of p.child.stdout) {
      size += chunk.length;
      if (size > 16 * 1024 * 1024) throw Error('原生目录响应超限');
      chunks.push(chunk);
    }
    active.throwIfAborted();
    if ((await p.finished) !== 0) throw Error(path.basename(exe) + ' 执行失败');
    return Buffer.concat(chunks).toString('utf8');
  } finally {
    await p.stop();
    await p.finished;
  }
}
async function* jsonLines(pipe) {
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let parts = [],
    size = 0;
  function append(part) {
    size += part.length;
    if (size > 128 * 1024 * 1024) throw Error('单条原生事件超过 128 MiB');
    if (part.length) parts.push(part);
  }
  function decode() {
    let value;
    try {
      const line = decoder.decode(parts.length === 1 ? parts[0] : Buffer.concat(parts, size));
      if (line.trim()) value = JSON.parse(line);
    } catch {
      throw Error('无效的原生 UTF-8 NDJSON');
    }
    parts = [];
    size = 0;
    return value;
  }
  for await (const chunk of pipe) {
    let offset = 0,
      next;
    while ((next = chunk.indexOf(10, offset)) >= 0) {
      append(chunk.subarray(offset, next));
      const event = decode();
      if (event !== undefined) yield event;
      offset = next + 1;
    }
    append(chunk.subarray(offset));
  }
  if (size) {
    const event = decode();
    if (event !== undefined) yield event;
  }
}
async function stream(exe, args, input, options, onEvent) {
  const p = start(exe, args, options);
  p.child.stdin.end(input);
  try {
    for await (const event of jsonLines(p.child.stdout)) await onEvent(event);
    options.signal?.throwIfAborted();
    if ((await p.finished) !== 0) throw Error(path.basename(exe) + ' 执行失败');
  } finally {
    await p.stop();
    await p.finished;
  }
}
const high = (profile) => profile === 'full' || profile === 'auto';
async function scope(agent, o, writer = false) {
  const profile = o.profile ?? 'legacy';
  if (!['legacy', 'readonly', 'ask', 'partial', 'full', 'auto'].includes(profile))
    throw Error('未知权限档位');
  o.signal?.throwIfAborted();
  if (high(profile)) {
    if (!o.writable) throw Error('完全访问与只读授权冲突');
    return 'danger-full-access';
  }
  if (!o.writable) return 'read-only';
  if (profile === 'readonly' || !writer) throw Error(agent + ' 未获受控写入权限');
  if (profile === 'ask') {
    if (
      typeof o.approve !== 'function' ||
      (await o.approve({
        agent,
        action: 'workspace_write',
        cwd: path.resolve(o.cwd),
        scope: 'current_turn',
        summary: '允许本轮在项目工作区写入并执行命令？'
      })) !== true
    )
      throw Error('本轮工作区写入未获批准');
    o.signal?.throwIfAborted();
  }
  return 'workspace-write';
}
function select(catalog, request) {
  const r = request ?? catalog.default;
  if (
    !r ||
    typeof r.provider !== 'string' ||
    typeof r.model !== 'string' ||
    r.mode !== 'cli' ||
    !catalog.modes.some((m) => m.id === r.mode && m.available === true)
  )
    throw Error('所选执行端不可自动执行');
  const p = catalog.providers.find((p) => p.id === r.provider && p.available !== false);
  if (!p || !p.models.some((m) => m.id === r.model && m.available !== false))
    throw Error('已选供应商或模型失效，请刷新配置');
  return { provider: r.provider, model: r.model, mode: r.mode };
}
function modes(label, available) {
  return [
    { id: 'cli', label, available: !!available, detail: '原生 CLI 协议' },
    { id: 'desktop', label: '桌面应用', available: false, detail: '桌面应用未打通自动执行和回传' }
  ];
}
async function emitBatch(emit, events) {
  if (typeof emit.batch === 'function') {
    await emit.batch(events);
    return;
  }
  for (const [kind, data] of events) await emit(kind, data);
}
module.exports = {
  fs,
  path,
  os,
  home,
  platformDir,
  readJSON,
  file,
  which,
  ccRows,
  ccWatchPaths,
  fingerprint,
  localHost,
  endpoint,
  getJSON,
  start,
  capture,
  jsonLines,
  stream,
  scope,
  high,
  select,
  modes,
  emitBatch
};
