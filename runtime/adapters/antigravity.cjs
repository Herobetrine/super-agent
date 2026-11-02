'use strict';
const {
  fs,
  path,
  home,
  platformDir,
  readJSON,
  file,
  which,
  ccRows,
  fingerprint,
  endpoint,
  capture,
  stream,
  scope,
  select,
  modes,
  emitBatch
} = require('./common.cjs');
const exe = () => which(process.platform === 'win32' ? 'agy.exe' : 'agy');
const nativeSettings = () => readJSON(path.join(home(), '.gemini/antigravity-cli/settings.json'));
function row(id, label, base, token, model) {
  const key = fingerprint('agy-', [id, base]);
  let safe = true;
  try {
    if (base) endpoint(base);
  } catch {
    safe = false;
  }
  const available = !!(typeof token === 'string' && token && safe && typeof model === 'string' && model);
  return {
    id: key,
    base,
    token,
    model,
    public: {
      id: key,
      label,
      models: typeof model === 'string' && model ? [{ id: model, label: model }] : [],
      defaultModel: typeof model === 'string' ? model : '',
      available,
      detail: available ? '本机保存的成对认证与端点' : '缺少配对凭据、端点或模型'
    }
  };
}
function configured() {
  const records = [],
    cfg = nativeSettings();
  if (cfg.modelProvider === 'gemini')
    records.push(
      row(
        'native',
        'Gemini API · 当前配置',
        process.env.GOOGLE_GEMINI_BASE_URL || '',
        process.env.GEMINI_API_KEY,
        cfg.model || process.env.GEMINI_MODEL
      )
    );
  for (const rec of ccRows('agy')) {
    try {
      const c = JSON.parse(rec.settings_config),
        env = c.env || {};
      records.push(
        row(
          rec.id,
          rec.name,
          env.GOOGLE_GEMINI_BASE_URL || '',
          env.GEMINI_API_KEY,
          c.config?.model?.name || env.GEMINI_MODEL
        )
      );
    } catch {
      records.push(row(rec.id, rec.name, '', null, null));
    }
  }
  return records;
}
function environment(provider, route, reader) {
  const env = { ...process.env },
    overrides = { modelProvider: '' };
  for (const key of ['GEMINI_API_KEY', 'GOOGLE_GEMINI_BASE_URL', 'AGY_ADC_AUTH']) delete env[key];
  if (route) {
    if (!route.public.available || provider !== route.id) throw Error('Antigravity 路由不可用');
    env.GEMINI_API_KEY = route.token;
    if (route.base) env.GOOGLE_GEMINI_BASE_URL = route.base;
    Object.assign(overrides, { modelProvider: 'gemini', model: route.model });
  } else if (provider === 'adc' && process.env.AGY_ADC_AUTH?.toLowerCase() === 'true')
    env.AGY_ADC_AUTH = 'true';
  else if (provider !== 'google') throw Error('Antigravity 供应商失效');
  if (reader) {
    const dir = path.join(platformDir(), provider === 'google' ? 'agy-reader' : 'agy-reader-' + provider);
    const target = path.join(dir, '.gemini/antigravity-cli/settings.json');
    overrides.permissions = {
      deny: ['write_file(*)', 'command(*)', 'mcp(*)', 'execute_url(*)', 'read_url(*)']
    };
    env.HOME = env.USERPROFILE = dir;
    if (JSON.stringify(readJSON(target)) !== JSON.stringify(overrides)) {
      fs.mkdirSync(path.dirname(target), { recursive: true });
      const tmp = target + '.' + require('node:crypto').randomUUID() + '.tmp';
      try {
        fs.writeFileSync(tmp, JSON.stringify(overrides), { flag: 'wx', mode: 0o600 });
        fs.renameSync(tmp, target);
      } finally {
        fs.rmSync(tmp, { force: true });
      }
    }
  }
  return { env, overrides };
}
async function discover(signal) {
  const cli = exe(),
    providers = [],
    settings = nativeSettings(),
    rows = configured();
  const ids = ['google', ...(process.env.AGY_ADC_AUTH?.toLowerCase() === 'true' ? ['adc'] : [])];
  for (const id of ids) {
    const models = [];
    if (cli) {
      try {
        const { env } = environment(id, null, true);
        const result = await capture(cli, ['models'], { env, timeout: 15000, signal });
        for (const line of result.split(/\r?\n/)) {
          const [mid, label] = line.split('\t');
          if (/^[\w./-]{1,150}$/.test(mid) && label?.trim()) models.push({ id: mid, label: label.trim() });
        }
        models.unshift({ id: '@native', label: '跟随 agy 原生默认模型' });
      } catch {
        signal?.throwIfAborted();
      }
    }
    providers.push({
      id,
      label: id === 'adc' ? 'Google Cloud ADC' : 'Google · Antigravity 登录账号',
      available: models.length > 0,
      models
    });
  }
  providers.push(...rows.map((r) => r.public));
  const selected =
    settings.modelProvider === 'gemini'
      ? rows[0]?.public
      : providers.find((p) => p.id === 'adc') || providers[0];
  const configuredModel = process.env.SUPERAGENT_AGY_MODEL || settings.model;
  const model =
    selected?.models.find((m) => m.id === configuredModel || m.label === configuredModel)?.id ||
    selected?.defaultModel ||
    (selected?.models.some((m) => m.id === '@native') ? '@native' : '');
  return {
    cli,
    rows,
    catalog: {
      providers,
      modes: modes('Antigravity CLI', cli),
      default: { mode: 'cli', provider: selected?.id || '', model },
      detail: 'Google 登录或保存的 Gemini 路由；目录按次读取。',
      downloads: [
        { label: 'Antigravity CLI 安装', url: 'https://antigravity.google/docs/cli/install' },
        { label: 'Antigravity 桌面版', url: 'https://antigravity.google/download' }
      ]
    }
  };
}
async function run(o) {
  const boundary = await scope('antigravity', o),
    { cli, rows, catalog } = await discover(o.signal),
    s = select(catalog, o.selection);
  const high = boundary === 'danger-full-access',
    r = rows.find((v) => v.id === s.provider);
  const { env, overrides } = environment(s.provider, r, !high);
  const args = [
    '--input-format',
    'stream-json',
    '--output-format',
    'stream-json',
    ...(o.sessionId ? ['--conversation', o.sessionId] : []),
    ...(s.model === '@native' ? [] : ['--model', s.model]),
    ...(high
      ? ['--dangerously-skip-permissions', '--settings', JSON.stringify(overrides)]
      : ['--disable-slash-commands'])
  ];
  let last;
  await o.emit('adapter.permission', { profile: o.profile, scope: boundary });
  await stream(
    cli,
    args,
    JSON.stringify({ event: 'user', message: { content: o.prompt } }) + '\n',
    { cwd: o.cwd, env, signal: o.signal },
    async (e) => {
      const frame = [['native', e]];
      if (e.event === 'step_update' && e.step_update?.step_type === 'agent_response') {
        if (e.step_update.text_delta) frame.push(['delta', e.step_update.text_delta]);
        if (e.step_update.state === 'DONE') frame.push(['message.boundary', null]);
      }
      await emitBatch(o.emit, frame);
      if (e.event === 'result') last = e.result;
    }
  );
  if (
    last?.status !== 'SUCCESS' ||
    typeof last.response !== 'string' ||
    !last.conversation_id ||
    (o.sessionId && o.sessionId !== last.conversation_id)
  )
    throw Error('Antigravity 未返回匹配会话的成功终态');
  return { text: last.response, sessionId: last.conversation_id };
}
function watchPaths() {
  const dir = path.join(home(), '.cc-switch'),
    rel = '.gemini/antigravity-cli/settings.json';
  return [
    ...require('./common.cjs').ccWatchPaths(),
    path.join(dir, 'settings.json'),
    path.join(dir, 'live-state.json'),
    path.join(home(), rel),
    path.join(platformDir(), 'agy-reader', rel),
    path.join(platformDir(), 'agy-reader-adc', rel),
    exe()
  ];
}
module.exports = { catalog: async () => (await discover()).catalog, run, watchPaths };
