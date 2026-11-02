'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { createHash } = require('node:crypto');
const { parse } = require('yaml');
function readSkill(folder, source, imported = false) {
  const dir = fs.realpathSync(folder),
    filename = fs.realpathSync(path.join(dir, 'SKILL.md'));
  if (path.dirname(filename) !== dir) throw Error('技能入口重定向到了其他目录');
  if (fs.statSync(filename).size > 128 * 1024) throw Error('技能入口超过 128 KiB');
  const body = fs.readFileSync(filename, 'utf8').replace(/^\uFEFF/, '');
  const meta = /^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(body);
  if (!meta) throw Error('未找到 SKILL.md 的 YAML 元数据');
  const header = parse(meta[1], { maxAliasCount: 0 });
  if (typeof header?.name !== 'string' || !header.name.trim() || typeof header.description !== 'string')
    throw Error('技能缺少名称或说明');
  return {
    id: createHash('sha256').update(filename).digest('hex'),
    name: header.name,
    description: header.description,
    source,
    imported,
    filename,
    body
  };
}
function scan(db, cwd) {
  const homes = [
    ['Codex', path.join(process.env.CODEX_HOME || path.join(os.homedir(), '.codex'), 'skills')],
    ['用户', path.join(os.homedir(), '.agents', 'skills')],
    ['Claude', path.join(os.homedir(), '.claude', 'skills')]
  ];
  if (cwd)
    homes.push(
      ['项目', path.join(cwd, '.agents', 'skills')],
      ['项目 Claude', path.join(cwd, '.claude', 'skills')]
    );
  const result = new Map();
  for (const [source, root] of homes) {
    if (!fs.existsSync(root)) continue;
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
      if (!entry.isDirectory() || entry.name.startsWith('.')) continue;
      try {
        const skill = readSkill(path.join(root, entry.name), source);
        result.set(skill.id, skill);
      } catch {
        /* Unusable skills are not offered. */
      }
    }
  }
  for (const { folder } of db.prepare('SELECT folder FROM studio_skill_dirs').all()) {
    try {
      const skill = readSkill(folder, '已导入', true);
      result.set(skill.id, skill);
    } catch {
      /* Removed source */
    }
  }
  return result;
}
module.exports = { scan, readSkill };
