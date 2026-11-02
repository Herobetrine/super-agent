'use strict';
const {
  path,
  os,
  home,
  platformDir,
  readJSON,
  file,
  which,
  ccRows,
  fingerprint,
  localHost,
  endpoint,
  getJSON,
  capture,
  stream,
  scope,
  select,
  modes,
  emitBatch
} = require('./common.cjs');
const nativeFile = () =>
  path.join(process.env.CLAUDE_CONFIG_DIR || path.join(home(), '.claude'), 'settings.json');
function executable() {
  const cli = which(process.platform === 'win32' ? 'claude.exe' : 'claude');
  if (cli) return cli;
  const shim = which('claude.cmd');
  const npm =
    shim && path.join(path.dirname(shim), 'node_modules/@anthropic-ai/claude-code/bin/claude.exe');
  return npm && file(npm) ? npm : null;
}
const apiBase = (base) => base.replace(/\/+$/, '').replace(/\/v1$/, '');
function officialEnv() {
  const env = { ...process.env };
  if (env.ANTHROPIC_BASE_URL) {
    delete env.ANTHROPIC_API_KEY;
    delete env.ANTHROPIC_AUTH_TOKEN;
  }
  for (const k of [
    'ANTHROPIC_BASE_URL',
    'ANTHROPIC_CUSTOM_HEADERS',
    'CLAUDE_CODE_USE_BEDROCK',
    'CLAUDE_CODE_USE_VERTEX',
    'CLAUDE_CODE_USE_FOUNDRY'
  ])
    delete env[k];
  const native = readJSON(nativeFile()).env || {};
  if (!native.ANTHROPIC_BASE_URL)
    for (const k of ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN']) if (native[k]) env[k] = native[k];
  return env;
}
async function firstParty(exe, signal) {
  const rec = {
    app: 'native',
    row: 'first-party',
    name: '官方账号',
    auth: 'sdk-login',
    base: '',
    models: [],
    reason: '未验证官方登录或模型目录'
  };
  if (!exe) return rec;
  const opts = { cwd: os.tmpdir(), env: officialEnv(), timeout: 8000, signal };
  try {
    const flags = ['--safe-mode', '--setting-sources', ''];
    const status = JSON.parse(await capture(exe, [...flags, 'auth', 'status'], opts));
    if (!status.loggedIn || status.apiProvider !== 'firstParty') return rec;
    const msg =
      JSON.stringify({
        type: 'control_request',
        request_id: 'catalog',
        request: { subtype: 'initialize' }
      }) + '\n';
    const buf = await capture(
      exe,
      [
        ...flags,
        '-p',
        '--input-format',
        'stream-json',
        '--output-format',
        'stream-json',
        '--verbose',
        '--strict-mcp-config',
        '--tools',
        '',
        '--no-session-persistence'
      ],
      { ...opts, input: msg }
    );
    for (const line of buf.trim().split(/\r?\n/)) {
      const r = JSON.parse(line).response;
      if (r?.request_id === 'catalog' && r.subtype === 'success')
        rec.models = r.response.models.map((m) => m.value).filter((x) => typeof x === 'string' && x);
    }
    if (rec.models.length) rec.reason = '';
  } catch {
    signal?.throwIfAborted();
  }
  return rec;
}
async function makeRecord(app, id, name, cfg, meta, cache, signal) {
  const env = cfg.env || {},
    base = (env.ANTHROPIC_BASE_URL || '').replace(/\/+$/, '');
  const auth = env.ANTHROPIC_AUTH_TOKEN ? 'ANTHROPIC_AUTH_TOKEN' : 'ANTHROPIC_API_KEY';
  const rec = {
    app,
    row: id,
    name,
    base,
    auth,
    token: env[auth],
    models: [],
    preferred: cfg.model || env.ANTHROPIC_MODEL || '',
    reason: ''
  };
  try {
    const url = endpoint(base);
    if (!rec.token || typeof rec.token !== 'string') throw Error('缺少配对凭据');
    if (meta.apiFormat && meta.apiFormat !== 'anthropic') throw Error('不兼容的供应商协议');
    const entries = [
      rec.preferred,
      ...Object.entries(env)
        .filter(([k]) => k === 'ANTHROPIC_MODEL' || /^ANTHROPIC_DEFAULT_\w+_MODEL$/.test(k))
        .map(([, v]) => v),
      ...Object.values(meta.claudeDesktopModelRoutes || {}).map((r) => r?.model)
    ];
    rec.models = [
      ...new Set(entries.filter((v) => typeof v === 'string' && v.length > 0 && v.length < 200))
    ];
    if (localHost(url.hostname)) {
      const key = JSON.stringify([base, auth, rec.token]);
      if (!cache.has(key))
        cache.set(
          key,
          getJSON(
            apiBase(base) + '/v1/models',
            auth === 'ANTHROPIC_API_KEY'
              ? { 'x-api-key': rec.token }
              : { Authorization: 'Bearer ' + rec.token },
            signal
          )
            .then((b) =>
              Array.isArray(b.data)
                ? b.data
                    .map((m) => m.id)
                    .filter((v) => typeof v === 'string' && v.length > 0 && v.length < 200)
                : null
            )
            .catch(() => null)
        );
      const discovered = await cache.get(key);
      if (discovered) rec.models = [...new Set(discovered)].sort();
    }
    if (!rec.models.length) throw Error('没有可用模型');
  } catch {
    rec.reason = '端点、凭据、协议或模型不可用';
  }
  return rec;
}
const rowId = (r) => fingerprint('ccs-', [r.app, r.row, r.base, r.auth]);
const route = (r) => ({
  provider: rowId(r),
  model: r.models.includes(r.preferred) ? r.preferred : r.models[0] || '',
  mode: 'cli'
});
async function discover(signal) {
  const exe = executable(),
    dir = path.join(home(), '.cc-switch'),
    device = readJSON(path.join(dir, 'settings.json')),
    state = readJSON(path.join(dir, 'live-state.json')).apps?.['claude-desktop'] || {},
    bound = readJSON(path.join(platformDir(), 'claude-route.json'));
  const official = await firstParty(exe, signal),
    cache = new Map();
  const records = await Promise.all(
    ccRows('claude', 'claude-desktop').map(async (row) => {
      try {
        const r = await makeRecord(
          row.app_type,
          row.id,
          row.name,
          JSON.parse(row.settings_config),
          row.meta ? JSON.parse(row.meta) : {},
          cache,
          signal
        );
        if (row.category === 'official' && !r.base)
          Object.assign(r, { auth: official.auth, models: official.models, reason: official.reason });
        return r;
      } catch {
        return {
          app: row.app_type,
          row: row.id,
          name: row.name,
          base: '',
          auth: '',
          models: [],
          reason: '无效的来源记录'
        };
      }
    })
  );
  const cfg = process.env.ANTHROPIC_BASE_URL ? { env: process.env } : readJSON(nativeFile());
  let native = await makeRecord('native', nativeFile(), '当前 Claude Code 配置', cfg, {}, cache, signal);
  if (native.base) {
    const match = records.find(
      (r) =>
        r.app === 'claude' &&
        !r.reason &&
        r.base === native.base &&
        r.auth === native.auth &&
        r.token === native.token
    );
    if (match) {
      if (match.models.includes(native.preferred)) match.preferred = native.preferred;
      native = match;
    } else records.push(native);
  } else {
    native = records.find((r) => r.app === 'claude' && r.auth === 'sdk-login') || official;
    if (!records.includes(native)) records.push(native);
  }
  const direct = !state.mode || state.mode === 'direct';
  const active = direct
    ? device.currentProviderClaudeDesktop
      ? records.find((r) => r.app === 'claude-desktop' && r.row === device.currentProviderClaudeDesktop)
      : native
    : null;
  const legacy = {};
  if (
    active &&
    !active.reason &&
    bound.app_type === active.app &&
    bound.provider_id === active.row &&
    (bound.base_url || '').replace(/\/+$/, '') === active.base &&
    active.models.includes(bound.model)
  ) {
    if (!active.preferred) active.preferred = bound.model;
    legacy['cc-switch'] = { ...route(active), model: bound.model };
  }
  if (!native.reason && native.preferred && native.models.includes(native.preferred))
    legacy.user = route(native);
  const providers = records.map((r) => ({
    id: rowId(r),
    label: r.name + ' · ' + r.app,
    available: !r.reason,
    models: r.models.map((id) => ({ id, label: id })),
    defaultModel: typeof r.preferred === 'string' ? r.preferred : '',
    detail: r.reason || '同一记录的端点与认证'
  }));
  return {
    exe,
    records,
    bound,
    catalog: {
      providers,
      default: active && !active.reason ? route(active) : { provider: '', model: '', mode: 'cli' },
      legacy_defaults: legacy,
      modes: modes('Claude Code CLI', exe),
      detail: 'CC Switch 已选直连来源及 Claude 原生配置',
      downloads: [
        { label: 'Claude Code 安装', url: 'https://code.claude.com/docs/en/quickstart' },
        { label: 'Claude 桌面版', url: 'https://claude.com/download' }
      ]
    }
  };
}
async function run(o) {
  const boundary = await scope('claude', o),
    high = boundary === 'danger-full-access';
  const { exe, records, bound, catalog } = await discover(o.signal),
    s = select(catalog, o.selection);
  const r = records.find((r) => rowId(r) === s.provider);
  const args = [
    '-p',
    '--output-format',
    'stream-json',
    '--input-format',
    'stream-json',
    '--verbose',
    '--include-partial-messages'
  ];
  if (high) args.push('--permission-mode', 'bypassPermissions');
  else
    args.push(
      '--safe-mode',
      '--strict-mcp-config',
      '--disable-slash-commands',
      '--no-chrome',
      '--tools',
      'Read,Glob,Grep',
      '--permission-mode',
      'dontAsk',
      '--setting-sources',
      ''
    );
  const env = r.auth === 'sdk-login' ? officialEnv() : { ...process.env };
  if (r.auth !== 'sdk-login') {
    for (const k of Object.keys(env))
      if (
        k.startsWith('ANTHROPIC_') ||
        k.startsWith('CLAUDE_CODE_OAUTH') ||
        [
          'CLAUDE_CODE_USE_BEDROCK',
          'CLAUDE_CODE_USE_VERTEX',
          'CLAUDE_CODE_USE_FOUNDRY',
          'SUPERAGENT_CLAUDE_AUTH_TOKEN'
        ].includes(k)
      )
        delete env[k];
    env.ANTHROPIC_BASE_URL = apiBase(r.base);
    env[r.auth] = r.token;
    if (apiBase(bound.base_url || '') === env.ANTHROPIC_BASE_URL && localHost(endpoint(r.base).hostname)) {
      const caps = env.CLAUDE_CODE_MODEL_CAPABILITIES?.replace(/;+$/, '');
      env.CLAUDE_CODE_MODEL_CAPABILITIES = (caps ? caps + ';' : '') + s.model + '=-mid_conv_system';
    }
  }
  if (high) env.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST = '1';
  args.push('--model', s.model);
  if (o.sessionId) args.push('--resume', o.sessionId);
  let terminal;
  await o.emit('adapter.permission', { profile: o.profile, scope: boundary });
  await stream(
    exe,
    args,
    JSON.stringify({ type: 'user', message: { role: 'user', content: o.prompt } }) + '\n',
    { cwd: o.cwd, env, signal: o.signal },
    async (e) => {
      const frame = [['native', e]];
      if (e.type === 'stream_event') {
        if (e.event?.delta?.type === 'text_delta') frame.push(['delta', e.event.delta.text]);
        if (e.event?.type === 'content_block_stop') frame.push(['message.boundary', null]);
      }
      await emitBatch(o.emit, frame);
      if (
        e.type === 'system' &&
        e.subtype === 'init' &&
        r.auth !== 'sdk-login' &&
        String(e.model).replace(/\[1M\]$/, '') !== s.model.replace(/\[1M\]$/, '')
      )
        throw Error('Claude 原生模型与所选路由不符');
      if (e.type === 'system' && e.subtype === 'api_retry' && e.error === 'authentication_failed')
        throw Error('Claude 供应商拒绝认证');
      if (e.type === 'result') terminal = e;
    }
  );
  if (
    !terminal ||
    terminal.is_error ||
    terminal.subtype !== 'success' ||
    typeof terminal.result !== 'string' ||
    !terminal.session_id ||
    (o.sessionId && terminal.session_id !== o.sessionId)
  )
    throw Error('Claude 未返回匹配会话的成功终态');
  return { text: terminal.result, sessionId: terminal.session_id };
}
function watchPaths() {
  const dir = path.join(home(), '.cc-switch'),
    native = nativeFile();
  return [
    ...require('./common.cjs').ccWatchPaths(),
    path.join(dir, 'settings.json'),
    path.join(dir, 'live-state.json'),
    native,
    path.join(path.dirname(native), '.credentials.json'),
    path.join(platformDir(), 'claude-route.json'),
    executable()
  ];
}
module.exports = { catalog: async () => (await discover()).catalog, run, watchPaths };
