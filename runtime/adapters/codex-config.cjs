'use strict';
const TOML = require('smol-toml');
const {
  fs,
  path,
  home,
  readJSON,
  file,
  which,
  localHost,
  endpoint,
  getJSON,
  capture,
  modes
} = require('./common.cjs');
const configHome = () => path.resolve(process.env.CODEX_HOME || path.join(home(), '.codex'));
function executable() {
  if (process.platform !== 'win32') return which('codex');
  const shim = which('codex.cmd');
  if (shim) {
    const vendor = path.join(
      path.dirname(shim),
      'node_modules/@openai/codex/node_modules/@openai',
      'codex-win32-' + process.arch,
      'vendor'
    );
    if (fs.existsSync(vendor))
      for (const arch of fs.readdirSync(vendor)) {
        const f = path.join(vendor, arch, 'bin/codex.exe');
        if (file(f)) return f;
      }
  }
  return which('codex.exe');
}
function toml(p) {
  try {
    return TOML.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    throw Error('无法读取 Codex 配置 ' + path.basename(p));
  }
}
function object(v) {
  return v && typeof v === 'object' && !Array.isArray(v);
}
function merge(a, b) {
  const c = Object.assign(Object.create(null), a);
  for (const [k, v] of Object.entries(b)) c[k] = object(v) && object(c[k]) ? merge(c[k], v) : v;
  return c;
}
function settings() {
  const dir = configHome(),
    f = path.join(dir, 'config.toml'),
    base = file(f) ? toml(f) : {};
  const profile = process.env.SUPERAGENT_CODEX_PROFILE?.trim();
  if (profile && !/^[A-Za-z0-9_-]+$/.test(profile)) throw Error('Codex profile 名无效');
  const config = profile ? merge(base, toml(path.join(dir, profile + '.config.toml'))) : base;
  const saved = [base, config];
  if (fs.existsSync(dir))
    for (const p of fs.readdirSync(dir).filter((n) => n.endsWith('.config.toml'))) {
      try {
        saved.push(merge(base, toml(path.join(dir, p))));
      } catch {}
    }
  return { config, profile, dir, saved };
}
const idOk = (id) => typeof id === 'string' && /^[\p{L}\p{N}_][\p{L}\p{N}_./:+-]{0,199}$/u.test(id);
const visible = (data) =>
  (Array.isArray(data?.models) ? data.models : [])
    .filter((m) => idOk(m.slug) && (m.visibility || 'list') === 'list')
    .map((m) => ({ id: m.slug, label: typeof m.display_name === 'string' ? m.display_name : m.slug }));
async function customModels(cfg, signal) {
  const origin = endpoint(cfg.base_url),
    u = cfg.model_catalog_url ? new URL(cfg.model_catalog_url) : null;
  if (u && (u.origin !== origin.origin || u.username || u.password || u.hash))
    throw Error('目录和认证来源不匹配');
  if (!u && !localHost(origin.hostname)) return [];
  const headers = { ...cfg.http_headers };
  for (const [name, ref] of Object.entries(cfg.env_http_headers || {}))
    if (process.env[ref]) headers[name] = process.env[ref];
  const token = cfg.experimental_bearer_token || process.env[cfg.env_key];
  if (token) headers.Authorization = 'Bearer ' + token;
  const data = await getJSON(u || origin.href.replace(/\/+$/, '') + '/models', headers, signal);
  return data.models
    ? visible(data)
    : (data.data || []).filter((m) => idOk(m.id)).map(({ id }) => ({ id, label: id }));
}
async function discover(signal) {
  const exe = executable(),
    { dir, config, profile, saved } = settings(),
    custom = config.model_providers || {},
    def = config.model_provider || 'openai';
  if (
    typeof def !== 'string' ||
    !/^[\w-]{1,96}$/.test(def) ||
    (config.model != null && !idOk(config.model))
  ) {
    throw Error('当前 Codex 配置含有无效的模型或供应商 ID');
  }
  const fileCatalog = config.model_catalog_json ? path.resolve(dir, config.model_catalog_json) : null;
  const providers = await Promise.all(
    [...new Set(['openai', ...Object.keys(custom), def])]
      .filter((n) => /^[\w-]{1,96}$/.test(n))
      .map(async (name) => {
        let cfg = Object.hasOwn(custom, name) ? custom[name] : null,
          models = [],
          detail = '没有该来源的模型目录';
        if (name === 'openai' && config.openai_base_url) cfg = { ...cfg, base_url: config.openai_base_url };
        try {
          if (fileCatalog && name === def) {
            if (fs.statSync(fileCatalog).size > 32 * 1024 * 1024) throw Error();
            models = visible(readJSON(fileCatalog));
            detail = '本机 model_catalog_json';
          } else if (cfg) {
            models = await customModels(cfg, signal);
            detail = models.length ? '配置来源的模型目录' : '未发现该来源的模型目录';
          } else if (name === 'openai' && exe) {
            models = visible(
              JSON.parse(
                await capture(
                  exe,
                  [
                    '-c',
                    'model_provider="openai"',
                    'debug',
                    'models',
                    ...(fileCatalog ? ['--bundled'] : [])
                  ],
                  { cwd: home(), signal, timeout: 8000 }
                )
              )
            );
            detail = 'Codex CLI 模型目录';
          }
        } catch {
          signal?.throwIfAborted();
          detail = '模型目录暂不可读';
        }
        const byId = new Map(models.map((m) => [m.id, m]));
        for (const c of saved)
          if (
            (c.model_provider || 'openai') === name &&
            idOk(c.model) &&
            !byId.has(c.model) &&
            JSON.stringify(c.model_providers?.[name]) === JSON.stringify(custom[name]) &&
            (name !== 'openai' || c.openai_base_url === config.openai_base_url)
          )
            byId.set(c.model, { id: c.model, label: c.model + '（本机配置）' });
        return {
          id: name,
          label: typeof cfg?.name === 'string' ? cfg.name : name,
          models: [...byId.values()],
          detail,
          available: byId.size > 0
        };
      })
  );
  const model = config.model || providers.find((p) => p.id === def)?.models[0]?.id || '';
  return {
    exe,
    config,
    profile,
    catalog: {
      providers,
      modes: modes('Codex CLI · app-server', exe),
      default: { provider: def, model, mode: 'cli' },
      detail: '当前 CODEX_HOME、profile 与原生模型目录',
      downloads: [
        { label: 'Codex CLI 安装', url: 'https://developers.openai.com/codex/cli/' },
        { label: 'Windows 桌面端', url: 'https://developers.openai.com/codex/app/windows/' }
      ]
    }
  };
}
function routing(d, s) {
  const { config, profile } = d,
    provider = s.provider,
    cfg = config.model_providers?.[provider];
  const args = ['-c', 'model_provider=' + JSON.stringify(provider)],
    settings = {};
  const override = provider === 'openai' && config.openai_base_url;
  const base = override || cfg?.base_url;
  if (base) {
    endpoint(base);
    args.push(
      '-c',
      (override ? 'openai_base_url' : 'model_providers.' + provider + '.base_url') +
        '=' +
        JSON.stringify(base)
    );
    if (profile && cfg)
      args.push('-c', 'model_providers.' + provider + '.name=' + JSON.stringify(provider));
  }
  if (profile) {
    if (cfg) settings.model_providers = { [provider]: cfg };
    if (override) settings.openai_base_url = base;
    if (config.model_catalog_json && provider === (config.model_provider || 'openai'))
      settings.model_catalog_json = path.resolve(configHome(), config.model_catalog_json);
  }
  return { args, settings };
}
function watchPaths() {
  const dir = configHome(),
    base = path.join(dir, 'config.toml');
  const paths = [base, path.join(dir, 'models_cache.json'), path.join(dir, 'auth.json')];
  const pointer = require('./metadata.cjs').pathField;
  let location = pointer('codex-base-catalog', base, () => toml(base).model_catalog_json);
  const profile = process.env.SUPERAGENT_CODEX_PROFILE?.trim();
  if (profile && /^[a-zA-Z0-9_-]+$/.test(profile)) {
    const selected = path.join(dir, profile + '.config.toml');
    paths.push(selected);
    const override = pointer('codex-selected-catalog', selected, () => toml(selected).model_catalog_json);
    if (override !== undefined) location = override;
  }
  try {
    paths.push(
      ...fs
        .readdirSync(dir)
        .filter((n) => n.endsWith('.config.toml'))
        .sort()
        .map((n) => path.join(dir, n))
    );
  } catch {
    /* Preset paths still detect creation of the base and selected profile. */
  }
  if (location) paths.push(path.resolve(dir, location));
  paths.push(executable());
  return paths;
}
module.exports = { discover, routing, watchPaths };
