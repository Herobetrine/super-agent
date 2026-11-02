'use strict';
const path = require('node:path');
const fs = require('node:fs');
const { randomUUID, createHash } = require('node:crypto');
const { Client } = require('@modelcontextprotocol/sdk/client/index.js');
const { StdioClientTransport } = require('@modelcontextprotocol/sdk/client/stdio.js');
const { StreamableHTTPClientTransport } = require('@modelcontextprotocol/sdk/client/streamableHttp.js');
const { scan, readSkill } = require('./skills.cjs');
const hash = (b) => createHash('sha256').update(b).digest('hex');
function check(ok, msg) {
  if (!ok) throw new Error(msg);
}
function nonempty(v, max) {
  return typeof v === 'string' && v.trim().length > 0 && v.length <= max && !v.includes('\0');
}
function choices(raw) {
  check(raw && Array.isArray(raw.skills) && Array.isArray(raw.servers), '扩展授权格式无效');
  for (const group of [raw.skills, raw.servers])
    check(group.length <= 16 && group.every((id) => nonempty(id, 100)), '每类最多启用 16 项');
  return { skills: [...new Set(raw.skills)], servers: [...new Set(raw.servers)] };
}
function executable(command) {
  check(nonempty(command, 1024), '请提供可执行程序');
  const absolute = path.isAbsolute(command);
  check(absolute || !/[\\/:]/.test(command), '命令必须是绝对路径或 PATH 中的程序名');
  const candidates = absolute
    ? [command]
    : (process.env.PATH || '')
        .split(path.delimiter)
        .filter(path.isAbsolute)
        .map((p) =>
          path.join(p, process.platform === 'win32' && !path.extname(command) ? command + '.exe' : command)
        );
  for (const p of candidates) {
    if (!fs.existsSync(p) || !fs.statSync(p).isFile()) continue;
    const real = fs.realpathSync(p);
    if (process.platform === 'win32')
      check(
        real.toLowerCase().endsWith('.exe'),
        'Windows 服务请指定 .exe；Node 服务可使用 node 并将入口脚本作为第一个参数'
      );
    return real;
  }
  throw Error('未找到可执行程序，请填写已安装程序的绝对路径');
}
function endpoint(value) {
  const url = new URL(value);
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname);
  check(
    (url.protocol === 'https:' || (local && url.protocol === 'http:')) &&
      !url.username &&
      !url.password &&
      !url.search &&
      !url.hash,
    '请使用不含凭据、查询或片段的 HTTPS 地址；回环服务可使用 HTTP'
  );
  return url;
}
function transportFor(config) {
  if (config.transport !== 'http')
    return new StdioClientTransport({
      command: config.command,
      args: config.args,
      stderr: 'ignore',
      cwd: path.dirname(config.command)
    });
  const url = endpoint(config.url);
  const token = config.token_env ? process.env[config.token_env] : null;
  check(!config.token_env || token, '未设置此 MCP 服务的令牌环境变量');
  return new StreamableHTTPClientTransport(url, {
    requestInit: { headers: token ? { Authorization: 'Bearer ' + token } : {} },
    fetch: (input, init) => {
      const target = new URL(typeof input === 'string' || input instanceof URL ? input : input.url);
      check(target.origin === url.origin, 'MCP 请求尝试离开已配置的来源');
      return fetch(input, { ...init, redirect: 'error' });
    }
  });
}
async function withClient(config, signal, use) {
  signal?.throwIfAborted();
  const transport = transportFor(config);
  const client = new Client({ name: 'super-agent-studio', version: '0.2.0' });
  try {
    await client.connect(transport, { timeout: 12000, signal });
    return await use(client);
  } finally {
    if (transport.sessionId)
      await Promise.race([
        transport.terminateSession().catch(() => {}),
        new Promise((r) => setTimeout(r, 2000).unref())
      ]);
    await client.close().catch(() => {});
  }
}
class Extensions {
  constructor(engine) {
    this.engine = engine;
    this.db = engine.db;
    this.db.exec(`CREATE TABLE IF NOT EXISTS studio_skill_dirs(id TEXT PRIMARY KEY,folder TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_servers(id TEXT PRIMARY KEY,name TEXT NOT NULL,command TEXT NOT NULL,args TEXT NOT NULL,
        enabled INTEGER NOT NULL,revision TEXT NOT NULL,tools TEXT,transport TEXT NOT NULL DEFAULT 'stdio',url TEXT NOT NULL DEFAULT '',token_env TEXT NOT NULL DEFAULT '');
      CREATE TABLE IF NOT EXISTS studio_extension_defaults(singleton INTEGER PRIMARY KEY CHECK(singleton=1),selected TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_ticket_extensions(ticket INTEGER PRIMARY KEY REFERENCES tickets,scope TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS studio_tool_receipts(job INTEGER NOT NULL REFERENCES jobs,logical INTEGER NOT NULL,key TEXT NOT NULL,
        request TEXT NOT NULL,result TEXT,state TEXT NOT NULL,PRIMARY KEY(job,logical,key));`);
  }
  defaults() {
    return JSON.parse(
      this.db.prepare('SELECT selected FROM studio_extension_defaults WHERE singleton=1').get()?.selected ||
        '{"skills":[],"servers":[]}'
    );
  }
  list(cwd) {
    return {
      skills: [...scan(this.db, cwd).values()].map(({ filename, body, ...meta }) => meta),
      servers: this.db
        .prepare(
          'SELECT id,name,command,args,enabled,tools,transport,url,token_env AS tokenEnv FROM studio_servers ORDER BY name'
        )
        .all()
        .map((r) => ({
          ...r,
          args: JSON.parse(r.args),
          enabled: !!r.enabled,
          tools: r.tools ? JSON.parse(r.tools).map((t) => t.name) : null
        })),
      defaults: this.defaults()
    };
  }
  setDefaults(raw, cwd) {
    const ids = choices(raw);
    this.prepare(ids, cwd);
    this.db.prepare('INSERT OR REPLACE INTO studio_extension_defaults VALUES(1,?)').run(JSON.stringify(ids));
  }
  clearDefault(group, id) {
    const d = this.defaults();
    d[group] = d[group].filter((x) => x !== id);
    this.db.prepare('INSERT OR REPLACE INTO studio_extension_defaults VALUES(1,?)').run(JSON.stringify(d));
  }
  import(folder) {
    const s = readSkill(folder, '已导入', true);
    this.db
      .prepare('INSERT OR REPLACE INTO studio_skill_dirs VALUES(?,?)')
      .run(s.id, path.dirname(s.filename));
    return s.id;
  }
  forget(id) {
    this.engine.tx(() => {
      this.db.prepare('DELETE FROM studio_skill_dirs WHERE id=?').run(id);
      this.clearDefault('skills', id);
    });
  }
  preview(id, cwd) {
    const s = scan(this.db, cwd).get(id);
    check(s, '技能不存在');
    return { name: s.name, body: s.body, source: s.source };
  }
  saveServer(row) {
    const { name, args = [], enabled = false } = row;
    check(
      nonempty(name, 100) &&
        typeof enabled === 'boolean' &&
        Array.isArray(args) &&
        args.length <= 64 &&
        args.every((s) => typeof s === 'string' && s.length <= 8192 && !s.includes('\0')),
      'MCP 服务参数无效'
    );
    const transport = row.transport ?? 'stdio';
    check(['stdio', 'http'].includes(transport), '不支持的 MCP 传输');
    const command = transport === 'stdio' ? executable(row.command) : '';
    const url = transport === 'http' ? endpoint(row.url).href : '';
    const tokenEnv = transport === 'http' ? row.tokenEnv || '' : '';
    check(!tokenEnv || /^MCP_[A-Z0-9_]{1,96}$/.test(tokenEnv), '请使用 MCP_ 开头的服务专用环境变量名');
    const old = row.id && this.db.prepare('SELECT * FROM studio_servers WHERE id=?').get(row.id);
    check(!row.id || old, '此服务已删除');
    const id = old?.id || randomUUID(),
      argv = JSON.stringify(args);
    const same =
      old?.args === argv &&
      old?.command === command &&
      old?.transport === transport &&
      old?.url === url &&
      old?.token_env === tokenEnv;
    // Changing launch parameters or disabling a service permanently revokes existing bindings.
    const revision = same && old.enabled === +enabled ? old.revision : randomUUID();
    this.engine.tx(() => {
      this.db
        .prepare(
          'INSERT OR REPLACE INTO studio_servers(id,name,command,args,enabled,revision,tools,transport,url,token_env) VALUES(?,?,?,?,?,?,?,?,?,?)'
        )
        .run(id, name, command, argv, +enabled, revision, same ? old.tools : null, transport, url, tokenEnv);
      if (!enabled || !same) this.clearDefault('servers', id);
    });
    return id;
  }
  remove(id) {
    this.engine.tx(() => {
      this.db.prepare('DELETE FROM studio_servers WHERE id=?').run(id);
      this.clearDefault('servers', id);
    });
  }
  async probe(id) {
    const s = this.db.prepare('SELECT * FROM studio_servers WHERE id=?').get(id);
    check(s, '未知服务');
    const tools = await withClient({ ...s, args: JSON.parse(s.args) }, undefined, async (client) => {
      const list = [];
      let cursor;
      const seen = new Set();
      do {
        const page = await client.listTools(cursor ? { cursor } : {}, { timeout: 10000 });
        for (const { name, description, inputSchema } of page.tools)
          list.push({ name, description: description || '', inputSchema });
        check(list.length <= 150 && JSON.stringify(list).length <= 256000, '工具目录超过上限');
        cursor = page.nextCursor;
        if (cursor) {
          check(!seen.has(cursor) && seen.size < 16, '工具分页异常');
          seen.add(cursor);
        }
      } while (cursor);
      return list;
    });
    check(
      this.db
        .prepare('UPDATE studio_servers SET tools=? WHERE id=? AND revision=?')
        .run(JSON.stringify(tools), id, s.revision).changes,
      '探测期间服务配置已改变'
    );
    return tools.map(({ name, description }) => ({ name, description }));
  }
  prepare(raw, cwd) {
    const d = choices(raw ?? this.defaults()),
      skills = [];
    if (d.skills.length) {
      const entries = scan(this.db, cwd);
      for (const id of d.skills) {
        const s = entries.get(id);
        check(s, '所选技能在当前项目不可用，请重新选择');
        skills.push({ id, name: s.name, source: s.filename, body: s.body, sha256: hash(s.body) });
      }
    }
    const servers = d.servers.map((id) => {
      const row = this.db.prepare('SELECT * FROM studio_servers WHERE id=? AND enabled=1').get(id);
      check(row && row.tools, '请先启用服务并连接读取工具');
      return { id, name: row.name, revision: row.revision, tools: JSON.parse(row.tools) };
    });
    return { skills, servers };
  }
  bind(ticket, scope) {
    this.db.prepare('INSERT INTO studio_ticket_extensions VALUES(?,?)').run(ticket, JSON.stringify(scope));
  }
  scope(ticket) {
    return JSON.parse(
      this.db.prepare('SELECT scope FROM studio_ticket_extensions WHERE ticket=?').get(ticket)?.scope ||
        '{"skills":[],"servers":[]}'
    );
  }
  summary(ticket) {
    const { skills, servers } = this.scope(ticket),
      brief = ({ id, name }) => ({ id, name });
    return { skills: skills.map(brief), servers: servers.map(brief) };
  }
  context(ticket, profile) {
    const { skills, servers } = this.scope(ticket);
    const mcp = ['readonly', 'legacy'].includes(profile)
      ? []
      : servers
          .filter((s) =>
            this.db
              .prepare('SELECT 1 FROM studio_servers WHERE id=? AND enabled AND revision=?')
              .get(s.id, s.revision)
          )
          .map((s) => ({ server: s.id, name: s.name, tools: s.tools }));
    return { skills, mcp };
  }
  results(job) {
    return this.db
      .prepare(
        `SELECT key,state,result FROM studio_tool_receipts WHERE job=? AND
      (logical=(SELECT max(logical) FROM studio_tool_receipts WHERE job=?) OR rowid IN
      (SELECT rowid FROM studio_tool_receipts WHERE job=? ORDER BY rowid DESC LIMIT 10)) ORDER BY rowid`
      )
      .all(job, job, job)
      .map((r) => ({
        id: r.key,
        state: r.state,
        result: r.result ? JSON.parse(r.result) : '此前调用已提交但结果未知。核对外部状态后再决定下一步。'
      }));
  }
  async invoke({ ticket, job, turn, logicalTurn, admission, profile, signal, approve, command }) {
    const { id, server, tool, arguments: args = {} } = command;
    check(
      nonempty(id, 96) &&
        nonempty(server, 100) &&
        nonempty(tool, 128) &&
        args &&
        typeof args === 'object' &&
        !Array.isArray(args),
      'MCP 调用格式无效'
    );
    const request = JSON.stringify({ server, tool, arguments: args });
    check(Buffer.byteLength(request) <= 64000, 'MCP 参数过大');
    const existing = this.db
      .prepare('SELECT request,state FROM studio_tool_receipts WHERE job=? AND logical=? AND key=?')
      .get(job, logicalTurn, id);
    if (existing) {
      check(existing.request === request, 'MCP 幂等标识已用于不同参数');
      return { newReceipt: false };
    }
    check(!['readonly', 'legacy'].includes(profile), '当前权限不允许 MCP 调用');
    const allowed = this.scope(ticket).servers.find(
      (s) => s.id === server && s.tools.some((t) => t.name === tool)
    );
    check(allowed, '此工具未获工单授权');
    const binding = () =>
      this.db
        .prepare('SELECT * FROM studio_servers WHERE id=? AND enabled AND revision=?')
        .get(server, allowed.revision);
    const jobRow = { ticket, id: job, turn };
    const current = () =>
      this.db
        .prepare(
          `SELECT 1 FROM jobs j WHERE id=? AND ticket=? AND turn=? AND state='running' AND parent IS NULL
      AND NOT EXISTS(SELECT 1 FROM events e WHERE e.ticket=j.ticket AND kind='user.follow' AND seq>?)`
        )
        .get(job, ticket, turn, admission ?? 0);
    signal.throwIfAborted();
    check(current(), '旧轮次已停止');
    this.db
      .prepare("INSERT INTO studio_tool_receipts VALUES(?,?,?,?,NULL,'pending')")
      .run(job, logicalTurn, id, request);
    let result,
      state,
      issued = false,
      returned = false;
    try {
      check(binding(), 'MCP 服务已禁用或已更改');
      if (!['full', 'auto'].includes(profile))
        check(
          await approve({
            tool: 'MCP ' + allowed.name + ' / ' + tool,
            arguments: args,
            detail: '该服务以当前用户执行，可能访问工作区外的文件和网络。'
          }),
          '用户未批准调用'
        );
      signal.throwIfAborted();
      check(current(), '旧轮次已停止');
      const s = binding();
      check(s, 'MCP 服务已禁用或已更改');
      result = await withClient({ ...s, args: JSON.parse(s.args) }, signal, (client) => {
        signal.throwIfAborted();
        check(current() && binding(), '指令或服务授权已改变');
        issued = true;
        return client.callTool({ name: tool, arguments: args }, undefined, { signal, timeout: 60000 });
      });
      returned = true;
      state = result.isError ? 'error' : 'done';
      if (Buffer.byteLength(JSON.stringify(result)) > 512 * 1024) {
        const raw = JSON.stringify(result),
          sha256 = hash(raw),
          folder = path.join(this.engine.dataDir, 'context-objects');
        fs.mkdirSync(folder, { recursive: true });
        const file = path.join(folder, sha256 + '.mcp.json');
        fs.writeFileSync(file, raw, { flush: true });
        result = { object_path: file, sha256, bytes: Buffer.byteLength(raw) };
      }
    } catch (e) {
      state = issued && !returned ? 'uncertain' : 'error';
      result = { error: e.message };
    }
    this.engine.tx(() => {
      this.db
        .prepare('UPDATE studio_tool_receipts SET state=?,result=? WHERE job=? AND logical=? AND key=?')
        .run(state, JSON.stringify(result), job, logicalTurn, id);
      const seq = this.engine.log(jobRow, 'mcp.result', { id, server, tool, state, result });
      if (returned && admission && current())
        this.db
          .prepare(
            `UPDATE continuous_runs SET probe=?,success=CASE WHEN ?='done' THEN ? ELSE success END
        WHERE ticket=? AND phase='discover' AND boundary_seq<?`
          )
          .run(seq, state, seq, ticket, admission);
    });
    return { newReceipt: true };
  }
}
module.exports = { Extensions };
