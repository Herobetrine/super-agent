'use strict';
const { path, fs } = require('./common.cjs');
const adapters = Object.freeze({
  codex: require('./codex.cjs'),
  claude: require('./claude.cjs'),
  deepseek: require('./deepseek.cjs'),
  antigravity: require('./antigravity.cjs')
});
function get(name) {
  if (!Object.hasOwn(adapters, name)) throw Error('未知原生执行器');
  return adapters[name];
}
async function catalog(agent) {
  return get(agent).catalog();
}
function catalogRevision(agent) {
  return require('./metadata.cjs').fingerprint(get(agent).watchPaths());
}
async function run(agent, options) {
  const adapter = get(agent);
  if (!options || typeof options.prompt !== 'string' || !options.prompt.trim()) throw Error('任务正文为空');
  if (
    typeof options.cwd !== 'string' ||
    !options.cwd.trim() ||
    !path.isAbsolute(options.cwd) ||
    !fs.statSync(options.cwd).isDirectory()
  )
    throw Error('项目目录无效');
  if (options.writable !== undefined && typeof options.writable !== 'boolean')
    throw Error('写入授权须为布尔值');
  if (
    options.sessionId != null &&
    (typeof options.sessionId !== 'string' || !/^[\w][\w:.-]{0,220}$/.test(options.sessionId))
  )
    throw Error('原生会话标识无效');
  if (
    options.selection != null &&
    (typeof options.selection !== 'object' ||
      options.selection.mode !== 'cli' ||
      typeof options.selection.provider !== 'string' ||
      typeof options.selection.model !== 'string')
  )
    throw Error('无效或不可执行的所选路由');
  options.signal?.throwIfAborted();
  return adapter.run({
    ...options,
    cwd: fs.realpathSync(options.cwd),
    writable: options.writable ?? false,
    emit: options.emit ?? (() => {})
  });
}
module.exports = { catalog, catalogRevision, run };
