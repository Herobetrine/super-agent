'use strict';
const {
  fs,
  path,
  os,
  home,
  platformDir,
  readJSON,
  file,
  which,
  ccRows,
  endpoint,
  capture,
  stream,
  scope,
  select,
  modes,
  emitBatch
} = require('./common.cjs');
const YAML = require('yaml');
const VERSION = '0.2.1-rc.5';
const runtime = () => process.env.SUPERAGENT_DSH_RUNTIME || path.join(platformDir(), 'dsh-runtime');
const dshHome = () => process.env.DSH_HOME || path.join(home(), '.dsh');
const node = () =>
  process.versions.electron ? which(process.platform === 'win32' ? 'node.exe' : 'node') : process.execPath;
const cli = () => path.join(runtime(), 'node_modules/@deepseek-ai/dsh/lib/bin.js');
function yaml(p, def) {
  if (!file(p)) return def;
  try {
    return YAML.parse(fs.readFileSync(p, 'utf8'), { logLevel: 'silent', maxAliasCount: 100 }) ?? def;
  } catch {
    throw Error('DSH YAML 配置无效：' + path.basename(p));
  }
}
function plugins(rows) {
  return (Array.isArray(rows) ? rows : []).flatMap((r) =>
    r && typeof r === 'object' ? [...plugins(r.insert), ...(r.id && !r.disabled ? [r] : [])] : []
  );
}
const nativeInfo = String.raw`import {supportedProtocols} from '@deepseek-ai/dsh-llm-pi-ai';
import {resolveAdapterOptions} from '@deepseek-ai/dsh-llm-deepseek-api-key';
import {readFileSync} from 'node:fs';
const {cfg,env}=JSON.parse(readFileSync(0,'utf8'));
const a=resolveAdapterOptions(cfg,{get:k=>Object.hasOwn(env,k)?{value:env[k]}:undefined});
console.log(JSON.stringify({protocols:supportedProtocols(),url:a.baseURL,ref:a.apiKeyEnv,
 models:a.models.map(({id,name,inputModalities,contextWindow})=>({id,name,inputModalities,contextWindow}))}));`;
async function info(cfg = {}, env = {}, signal) {
  return JSON.parse(
    await capture(node(), ['--input-type=module', '-e', nativeInfo], {
      cwd: runtime(),
      input: JSON.stringify({ cfg, env }),
      signal
    })
  );
}
function models(data) {
  const rows = Array.isArray(data)
    ? data
    : Object.entries(data || {})
        .filter(([k, v]) => v?.id === k)
        .map(([, v]) => v);
  return new Map(rows.filter((r) => r && typeof r.id === 'string' && r.id).map((r) => [r.id, r]));
}
const binding = (c) => JSON.stringify([c.api, c.baseURL, c.apiKeyEnv]);
async function discover(signal) {
  const h = dshHome(),
    routes = new Map(),
    native = new Map(),
    defaults = {},
    official = {};
  let builtin,
    ok = !!node() && file(cli()) && readJSON(path.resolve(cli(), '../../package.json')).version === VERSION;
  const routesFile = path.join(platformDir(), 'deepseek-default.json');
  const baseFile = path.join(runtime(), 'node_modules/@deepseek-ai/dsh-base/cordis.patch.yml');
  for (const p of [
    baseFile,
    ...['profiles/desktop/cordis.patch.yml', 'profiles/headless/cordis.patch.yml', 'cordis.patch.yml'].map(
      (x) => path.join(h, x)
    )
  ]) {
    for (const plug of plugins(yaml(p, []))) {
      const cfg = plug.config || {};
      if (plug.id === 'agent-default-model') {
        for (const k of ['model', 'provider']) if (typeof cfg[k] === 'string') defaults[k] = cfg[k];
        if (p === baseFile) builtin = defaults.provider;
      }
      if (plug.id === 'llm-pi-ai')
        for (const [id, v] of Object.entries(cfg.providers || {})) native.set(id, v);
      if (plug.id === 'llm-deepseek') Object.assign(official, cfg);
    }
  }
  const settings = yaml(path.join(h, 'settings.yaml'), {});
  for (const [id, v] of Object.entries(settings['llm-pi-ai']?.providers || {})) native.set(id, v);
  Object.assign(defaults, settings['agent-default-model'] || {});
  Object.assign(official, settings['llm-deepseek'] || {});
  const store = path.join(h, '.credentials.yaml'),
    refs = yaml(store, {}).refs || {};
  let dotenv = {};
  if (file(path.join(h, '.env')))
    dotenv = require('node:util').parseEnv(fs.readFileSync(path.join(h, '.env'), 'utf8'));
  const env = { ...dotenv, ...process.env };
  const cleanCfg = Object.fromEntries(
    ['baseURL', 'models', 'apiKeyEnv'].filter((k) => official[k] !== undefined).map((k) => [k, official[k]])
  );
  if (ok) {
    const base = await info({}, {}, signal);
    try {
      const overrides = Object.fromEntries(
        Object.entries(env).filter(([k]) => /_(BASE_URL|BASEURL)$/.test(k))
      );
      const spec =
        Object.keys(cleanCfg).length || Object.keys(overrides).length
          ? await info(cleanCfg, overrides, signal)
          : base;
      const u = endpoint(spec.url);
      const paired =
        u.origin === endpoint(base.url).origin || (cleanCfg.apiKeyEnv && spec.ref !== base.ref);
      if (builtin && paired && (env[spec.ref] || Object.hasOwn(refs, spec.ref)))
        routes.set(builtin, {
          official: true,
          label: 'DeepSeek 原生 API',
          url: spec.url,
          ref: spec.ref,
          key: env[spec.ref],
          store,
          models: models(spec.models)
        });
    } catch {
      signal?.throwIfAborted();
    }
    const records = ccRows('dsh'),
      ccids = new Set(records.map((r) => r.id));
    const candidates = [];
    for (const row of records) {
      try {
        const cfg = JSON.parse(row.settings_config),
          o = cfg.options || {};
        if (
          (o.apiKey != null && (typeof o.apiKey !== 'string' || !o.apiKey.trim())) ||
          ('baseURL' in o && o.baseURL !== cfg.baseURL) ||
          (o.headers && Object.keys(o.headers).length)
        )
          continue;
        candidates.push([row.id, row.name, cfg, o.apiKey]);
      } catch {}
    }
    for (const [id, cfg] of native) if (!ccids.has(id)) candidates.push([id, cfg.displayName || id, cfg]);
    for (const [id, label, cfg, credential] of candidates) {
      try {
        const ref = cfg.apiKeyEnv;
        if (
          id === builtin ||
          !base.protocols.includes(cfg.api) ||
          typeof ref !== 'string' ||
          !/^[A-Za-z_]\w*$/.test(ref) ||
          (cfg.headers && Object.keys(cfg.headers).length)
        )
          continue;
        endpoint(cfg.baseURL);
        const key = credential || env[ref],
          peer = native.get(id);
        if ((!key && !Object.hasOwn(refs, ref)) || (peer && binding(peer) !== binding(cfg))) continue;
        const entries = new Map([...models(peer?.models), ...models(cfg.models)]);
        if (entries.size)
          routes.set(id, {
            label,
            url: cfg.baseURL,
            api: cfg.api,
            ref,
            key,
            store,
            models: entries,
            reasoning: cfg.reasoning
          });
      } catch {}
    }
  }
  const pref = readJSON(routesFile, {
    mode: 'cli',
    provider: defaults.provider || '',
    model: defaults.model || ''
  });
  const valid =
    pref &&
    Object.keys(pref).sort().join(',') === 'mode,model,provider' &&
    ['mode', 'model', 'provider'].every((k) => typeof pref[k] === 'string' && pref[k]);
  const def = valid ? { mode: pref.mode, provider: pref.provider, model: pref.model } : {};
  const catalog = {
    default: def,
    providers: [...routes].map(([id, r]) => ({
      id,
      label: typeof r.label === 'string' ? r.label : id,
      models: [...r.models.values()].map((m) => ({
        id: m.id,
        label: typeof m.name === 'string' ? m.name : m.id
      }))
    })),
    modes: modes('DeepSeek Harness CLI', ok),
    downloads: [
      {
        label: 'DeepSeek Harness Windows 桌面版',
        url: 'https://download.deepseek.com/desktop/dsh-latest-windows-x64.exe'
      },
      { label: 'DeepSeek Harness 官网', url: 'https://www.deepseek.com/zh/harness/' },
      { label: 'Harness CLI 安装', url: 'https://github.com/deepseek-ai/deepseek-harness#run-from-npm' }
    ],
    detail: '本机 DSH / CC Switch 与已保存工单默认来源，重新读取时生效。'
  };
  return { catalog, routes };
}
async function run(o) {
  const boundary = await scope('deepseek', o, true);
  const { catalog, routes } = await discover(o.signal),
    s = select(catalog, o.selection),
    r = routes.get(s.provider);
  const env = {
    ...process.env,
    DSH_HOME: path.join(platformDir(), 'dsh-state', boundary),
    DSH_PERMISSION_MODE: boundary
  };
  let ref = r.ref;
  if (r.key) {
    ref = 'SUPERAGENT_DSH_CALL_KEY';
    env[ref] = r.key;
  }
  const allowed = ['id', 'name', 'contextWindow', 'input', 'reasoningEfforts', 'maxTokens', 'compat'];
  const selected = Object.fromEntries(
    Object.entries(r.models.get(s.model)).filter(([k]) => allowed.includes(k))
  );
  const spec = { baseURL: r.url, apiKeyEnv: ref, models: r.official ? [...r.models.values()] : [selected] };
  if (!r.official) {
    spec.api = r.api;
    if (r.reasoning) spec.reasoning = r.reasoning;
  }
  let policy =
    YAML.stringify([{ id: 'agent-default-model', config: { provider: s.provider, model: s.model } }]) +
    '- id: sandbox-policy\n  config:\n    mode: ' +
    boundary +
    '\n    workspaceRoot: !!js process.cwd()\n' +
    YAML.stringify([
      { id: 'approval', config: { policy: 'never' } },
      {
        id: 'permission',
        config: {
          defaultPreset: boundary,
          presets: { [boundary]: { sandbox: boundary, approval: 'never' } }
        }
      },
      { id: r.official ? 'llm-pi-ai' : 'llm-deepseek', disabled: true },
      {
        id: r.official ? 'llm-deepseek' : 'llm-pi-ai',
        config: r.official ? spec : { providers: { [s.provider]: spec } }
      }
    ]);
  if (!r.key)
    policy += YAML.stringify([
      { id: 'credentials', disabled: false, config: { path: r.store, watch: false } }
    ]);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-dsh-')),
    patch = path.join(temp, 'policy.yml');
  let text, sessionId;
  try {
    fs.writeFileSync(patch, policy, { mode: 0o600 });
    const args = [
      '--expose-internals',
      cli(),
      '--profile',
      'headless',
      '--patch',
      patch,
      '--json',
      ...(o.sessionId ? ['--session-id', o.sessionId] : []),
      '-'
    ];
    await o.emit('adapter.permission', { profile: o.profile, scope: boundary });
    await o.emit('adapter.route', { provider: s.provider, requested_model: s.model, mode: 'cli' });
    await stream(node(), args, o.prompt, { cwd: o.cwd, env, signal: o.signal }, async (e) => {
      if (e.type === 'thinking') return;
      const frame = [['native', e]];
      if (e.type === 'session') sessionId = e.sessionId;
      if (e.type === 'text' && typeof e.text === 'string')
        frame.push(['delta', e.text], ['message.boundary', null]);
      await emitBatch(o.emit, frame);
      if (e.type === 'final') text = e.text;
    });
  } finally {
    fs.rmSync(patch, { force: true });
    fs.rmdirSync(temp);
  }
  if (typeof text !== 'string' || !text || !sessionId || (o.sessionId && sessionId !== o.sessionId))
    throw Error('DeepSeek 未返回匹配会话的完整终稿');
  return { text, sessionId };
}
function watchPaths() {
  const h = dshHome(),
    pkg = path.join(runtime(), 'node_modules/@deepseek-ai');
  return [
    ...require('./common.cjs').ccWatchPaths(),
    path.join(platformDir(), 'deepseek-default.json'),
    ...[
      '.credentials.yaml',
      '.env',
      'settings.yaml',
      'profiles/desktop/cordis.patch.yml',
      'profiles/headless/cordis.patch.yml',
      'cordis.patch.yml'
    ].map((n) => path.join(h, n)),
    path.join(pkg, 'dsh-base/cordis.patch.yml'),
    cli(),
    path.join(pkg, 'dsh/package.json'),
    node(),
    ...['dsh-llm-deepseek', 'dsh-llm-deepseek-api-key', 'dsh-llm-pi-ai'].flatMap((name) => [
      path.join(pkg, name, 'package.json'),
      path.join(pkg, name, 'lib/index.js')
    ])
  ];
}
module.exports = { catalog: async () => (await discover()).catalog, run, watchPaths };
