'use strict';

const P = require('./engine-protocol.cjs');

function check(store, ctx, cmd) {
  const { job, catalog } = ctx;
  if (!P.isObj(cmd)) throw new Error('SA_SEND 需要 JSON 对象');
  const { id, to, body, write = false, role = '', files = [] } = cmd;
  if (
    ['provider', 'mode', 'selection', 'route', 'route_override', 'permission_override'].some((k) =>
      Object.hasOwn(cmd, k)
    )
  )
    throw new Error('不可通过派工更换供应商或执行端');
  if (
    !P.validText(id, 96) ||
    !P.AGENTS.includes(to) ||
    !P.validText(body) ||
    typeof write !== 'boolean' ||
    typeof role !== 'string' ||
    role.length > 120 ||
    !Array.isArray(files)
  )
    throw new Error('派工参数无效');
  const hasModel = Object.hasOwn(cmd, 'model'),
    hasProfile = Object.hasOwn(cmd, 'permission_profile');
  if ((hasModel || hasProfile) && (job.parent !== null || !job.allow_lead_routing))
    throw new Error('只有获授权的主控可指定子任务模型和权限');
  if (hasProfile && !P.childProfiles(job.profile).includes(cmd.permission_profile))
    throw new Error('子任务权限不能超出上级范围');
  if (hasModel && !P.ident(cmd.model)) throw new Error('模型标识无效');
  const override = hasProfile ? cmd.permission_profile : job.permission_override;
  const effective = override || job.profile;
  const logical = job.dispatch_turn || job.turn;
  const old = store.one(
    'SELECT * FROM jobs WHERE parent=? AND source_turn=? AND msgkey=?',
    job.id,
    logical,
    id
  );
  if (old) {
    const manifest = store.one('SELECT manifest FROM job_files WHERE job=?', old.id);
    const names = manifest ? JSON.parse(manifest.manifest).map((f) => f.name) : [];
    if (
      old.agent !== to ||
      old.body !== body ||
      !!old.writing !== write ||
      old.assigned_role !== role ||
      (JSON.parse(old.route_override || 'null')?.model ?? null) !== (cmd.model ?? null) ||
      old.permission_override !== override ||
      JSON.stringify(files) !== JSON.stringify(names)
    )
      throw new Error('同一消息 ID 携带不同的任务、模型、附件或权限');
    return { old };
  }
  let selection = null;
  if (hasModel) {
    const menu = catalog[to];
    if (!menu?.models.includes(cmd.model)) throw new Error('所选模型不在此任务的供应商和执行端目录');
    selection = P.route({ provider: menu.provider, mode: menu.mode, model: cmd.model });
    const pinned = JSON.parse(job.agent_routes)[to];
    if (pinned && (pinned.provider !== selection.provider || pinned.mode !== selection.mode))
      throw new Error('工单路由已变更');
  }
  if (write && (!job.writes || effective === 'readonly' || !P.canWrite(to, effective)))
    throw new Error('该子任务没有所请求的写权限');
  return { selection, effective, override, logical, id, to, body, write, role, files };
}

async function dispatch(store, ctx, cmd) {
  const { job, signal } = ctx;
  const superseded = () =>
    job.continuous_ticket &&
    job.parent === null &&
    store.one(
      "SELECT 1 FROM events WHERE ticket=? AND kind='user.follow' AND seq>? LIMIT 1",
      job.ticket,
      job.admission
    );
  if (signal.aborted || !store.live(job) || superseded()) return;
  const first = check(store, ctx, cmd);
  if (first.old) return;
  const manifest = await store.snapshot(job.cwd, first.files);
  // Never hold the DB writer while the native filesystem worker does I/O.
  // Both authority and deduplication are checked again at the commit point.
  if (signal.aborted) return;
  return store.tx(() => {
    if (!store.live(job) || superseded()) return;
    const now = check(store, ctx, cmd);
    if (now.old) return;
    const { id, to, body, write, role, selection, effective, override, logical } = now;
    const child = Number(
      store.run(
        `INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,source_turn,
      msgkey,state,role,assigned_role,route_override,permission_override) VALUES(?,?,?,?,?,?,?,?,?,'queued',?,?,?,?)`,
        job.ticket,
        job.id,
        job.agent,
        to,
        body,
        +write,
        job.depth + 1,
        logical,
        id,
        role,
        role,
        selection ? JSON.stringify(selection) : null,
        override
      ).lastInsertRowid
    );
    if (manifest.length) store.run('INSERT INTO job_files VALUES(?,?)', child, JSON.stringify(manifest));
    store.log(job, 'send', {
      to,
      child,
      body,
      sha256: P.digest(body),
      write,
      files: manifest,
      role,
      selection,
      permission_profile: effective
    });
  });
}

function progress(store, { job, signal }, json) {
  store.tx(() => {
    if (
      signal.aborted ||
      !store.live(job) ||
      store.one(
        "SELECT 1 FROM events WHERE ticket=? AND kind='user.follow' AND seq>? LIMIT 1",
        job.ticket,
        job.admission
      )
    )
      return;
    // Reject optional updates without quoting the malformed content or failing the agent's assignment.
    const reject = (reason) => store.log(job, 'protocol.progress_invalid', { reason });
    let cmd;
    try {
      cmd = JSON.parse(json);
    } catch {
      return reject('invalid_json');
    }
    if (
      !P.isObj(cmd) ||
      Object.keys(cmd).length !== 2 ||
      !P.ident(cmd.id) ||
      cmd.id.length > 96 ||
      !P.validText(cmd.summary, 320) ||
      /[\p{Cc}\p{Zl}\p{Zp}\u202a-\u202e\u2066-\u2069]/u.test(cmd.summary)
    )
      return reject('invalid_fields');
    const logical = job.dispatch_turn || job.turn;
    const old = store.one(
      'SELECT e.payload FROM progress_receipts p JOIN events e ON e.seq=p.seq WHERE p.job=? AND p.source_turn=? AND p.msgkey=?',
      job.id,
      logical,
      cmd.id
    );
    if (old) {
      if (JSON.parse(old.payload).summary !== cmd.summary) return reject('conflicting_id');
      return;
    }
    const seq = store.log(job, 'job.progress', { id: cmd.id, summary: cmd.summary });
    store.run(
      'INSERT INTO progress_receipts(job,source_turn,msgkey,seq) VALUES(?,?,?,?)',
      job.id,
      logical,
      cmd.id,
      seq
    );
  });
}

async function command(store, ctx, line) {
  if (ctx.signal.aborted) return;
  const sep = line.indexOf(' ');
  if (sep < 0) return;
  const tag = line.slice(0, sep);
  if (tag === 'SA_PROGRESS') return progress(store, ctx, line.slice(sep + 1));
  if (!['SA_ROLE', 'SA_SEND', 'SA_NOTE', 'SA_TOOL'].includes(tag)) return;
  const cmd = JSON.parse(line.slice(sep + 1));
  if (tag === 'SA_SEND') return dispatch(store, ctx, cmd);
  if (tag === 'SA_TOOL') {
    const { job, signal } = ctx;
    if (!store.live(job) || signal.aborted) return;
    if (job.parent !== null || !store.extensions?.invoke || ['readonly', 'legacy'].includes(job.profile))
      throw new Error('当前工单或执行者不可调用 MCP 工具');
    if (
      !P.isObj(cmd) ||
      !P.validText(cmd.id, 96) ||
      !P.ident(cmd.server) ||
      !P.ident(cmd.tool) ||
      !P.isObj(cmd.arguments)
    )
      throw new Error('SA_TOOL 需要 id、server、tool 和 arguments 对象');
    const advertised = ctx.packet.project_extensions?.mcp || [];
    if (!advertised.some((s) => s.server === cmd.server && s.tools?.some((t) => t.name === cmd.tool)))
      throw new Error('工具不在本次工单启用的服务目录内');
    const result = await store.extensions.invoke({
      ticket: job.ticket,
      job: job.id,
      turn: job.turn,
      admission: job.admission,
      logicalTurn: job.dispatch_turn || job.turn,
      profile: job.profile,
      cwd: job.cwd,
      signal,
      approve: (action) => store.approve(ctx, action),
      command: cmd
    });
    if (result?.newReceipt) ctx.toolFollow = true;
    return;
  }
  store.tx(() => {
    const { job } = ctx;
    if (!store.live(job)) return;
    if (tag === 'SA_ROLE') {
      if (!P.validText(cmd?.role, 120)) throw new Error('无效的职能名称');
      if (store.run('UPDATE jobs SET role=? WHERE id=? AND role!=?', cmd.role, job.id, cmd.role).changes)
        store.log(job, 'role', { role: cmd.role });
    } else {
      if (!P.validText(cmd?.id, 96)) throw new Error('无效的信件 ID');
      store.recordNote(job.ticket, cmd.job, cmd.body, job, cmd.id);
    }
  });
}
module.exports = { command };
