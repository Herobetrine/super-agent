'use strict';
const fs = require('node:fs');
const fsp = require('node:fs/promises');
const { DatabaseSync } = require('node:sqlite');
const { createHash } = require('node:crypto');
const { spawn } = require('node:child_process');
const path = require('node:path');
const os = require('node:os');
const { once } = require('node:events');
const { finished } = require('node:stream/promises');
const { which } = require('./adapters/common.cjs');
const { ProgressJournal } = require('./progress.cjs');

const AGENTS = ['codex', 'claude', 'deepseek', 'antigravity'];
const VISIBLE = [
  'user',
  'user.follow',
  'delta',
  'reply',
  'send',
  'role',
  'note',
  'note.rejected',
  'failed',
  'cancelled',
  'interrupted',
  'auto.goal',
  'auto.next',
  'auto.learn',
  'auto.milestone',
  'auto.discovered',
  'auto.exhausted',
  'auto.retry',
  'auto.backoff',
  'auto.finish',
  'auto.recover',
  'approval.request',
  'approval.decision',
  'mcp.result'
];
const JOB_DETAILS = `SELECT j.id,agent,parent,role,state,turn,writing,ready_after,permission_override,
  substr(body,1,8000) body,substr(result,1,5000) result,n.route FROM jobs j LEFT JOIN native_routes n ON n.job=j.id`;
function jobView(row) {
  return { ...row, route: row.route ? JSON.parse(row.route) : null };
}
function expect(ok, msg) {
  if (!ok) throw new Error(msg);
}
function text(v, max = 200000) {
  expect(typeof v === 'string' && v.length <= max, '文本长度或格式无效');
  return v;
}
function id(v) {
  expect(Number.isSafeInteger(v) && v > 0, '编号无效');
  return v;
}
function parseRoutes(routes) {
  expect(routes && typeof routes === 'object' && !Array.isArray(routes), '路由格式有误');
  for (const [agent, row] of Object.entries(routes)) {
    expect(
      AGENTS.includes(agent) && row && Object.keys(row).sort().join(',') === 'mode,model,provider',
      '路由字段无效'
    );
    expect(
      ['cli', 'desktop'].includes(row.mode) && text(row.provider, 1000) && text(row.model, 1000),
      '模型或供应商无效'
    );
  }
  return routes;
}

function git(cwd, args) {
  return new Promise((resolve, reject) => {
    const exe = which(process.platform === 'win32' ? 'git.exe' : 'git');
    expect(exe, '未在 PATH 中找到 Git');
    const cp = spawn(
      exe,
      ['--no-pager', '--no-optional-locks', '-c', 'core.fsmonitor=false', '-c', 'color.ui=false', ...args],
      { cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }
    );
    const chunks = [];
    let size = 0,
      cut = false,
      stderr = '';
    const timer = setTimeout(() => cp.kill(), 15000);
    cp.on('error', (e) => {
      clearTimeout(timer);
      reject(e);
    });
    cp.stdout.on('data', (chunk) => {
      const space = 256 * 1024 - size;
      if (space > 0) {
        const part = chunk.subarray(0, space);
        chunks.push(part);
        size += part.length;
      }
      if (chunk.length > space) {
        cut = true;
        cp.kill();
      }
    });
    cp.stderr.on('data', (b) => {
      if (stderr.length < 4096) stderr += b.toString();
    });
    cp.on('close', (code) => {
      clearTimeout(timer);
      if (code !== 0 && !cut) reject(new Error(stderr.trim() || 'Git 检查中断'));
      else resolve(Buffer.concat(chunks).toString('utf8') + (cut ? '\n…预览已截断' : ''));
    });
  });
}

class Studio {
  constructor(engine, native, adapters, dataDir, {readOnly=false}={}) {
    Object.assign(this, { engine, native, adapters, dataDir, db: engine.db });
    this.roots = new Set();
    this.progress=new ProgressJournal(this.db);
    if(!readOnly)this.db.exec(`CREATE TABLE IF NOT EXISTS studio_plan(id INTEGER PRIMARY KEY,
      ticket INTEGER NOT NULL REFERENCES tickets,body TEXT NOT NULL,done INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS studio_plan_ticket ON studio_plan(ticket,id)`);
  }
  authorize(cwd) {
    const root = fs.realpathSync.native(cwd);
    expect(fs.statSync(root).isDirectory(), '项目目录不存在');
    this.roots.add(root);
    return root;
  }
  project(cwd) {
    text(cwd, 32768);
    const known =
      this.roots.has(cwd) || this.db.prepare('SELECT 1 FROM tickets WHERE cwd=? LIMIT 1').get(cwd);
    expect(known, '请先用目录选择器选中项目');
    return fs.realpathSync.native(cwd);
  }
  history({ q = '' }) {
    const pattern = '%' + text(q, 512).replace(/[\\%_]/g, (s) => '\\' + s) + '%';
    return this.db
      .prepare(
        `SELECT t.id,substr(t.goal,1,500) goal,t.cwd,t.lead,t.created,j.state
      FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL
      WHERE t.goal LIKE ? ESCAPE '\\' OR t.cwd LIKE ? ESCAPE '\\' ORDER BY t.id DESC LIMIT 150`
      )
      .all(pattern, pattern);
  }
  jobsPage({ ticket, before }) {
    const cursor = before == null ? Number.MAX_SAFE_INTEGER : id(before);
    const rows = this.db
      .prepare(JOB_DETAILS + ' WHERE ticket=? AND j.id<? ORDER BY j.id DESC LIMIT 81')
      .all(id(ticket), cursor);
    const jobs = rows.slice(0, 80).map(jobView);
    return { jobs, more: rows.length > 80, next: jobs.at(-1)?.id ?? null };
  }
  async activityPage({ticket,before}) {
    id(ticket);
    const high=before==null?Number.MAX_SAFE_INTEGER:id(before)-1;
    const end=this.db.prepare('SELECT coalesce(max(seq),0) n FROM events WHERE ticket=? AND seq<=?').get(ticket,high).n;
    await this.progress.ensure(ticket,end);
    const page=this.progress.read(ticket,0,end,true,81);
    const events=page.slice(0,80);
    return {events,more:page.length>80,next:events.at(-1)?.seq??null};
  }
  async snapshot({ ticket, after = 0 }) {
    id(ticket);
    expect(Number.isSafeInteger(after) && after >= 0, '事件游标无效');
    const end = this.db.prepare('SELECT coalesce(max(seq),0) n FROM events WHERE ticket=?').get(ticket).n;
    const native=await this.progress.ensure(ticket,end);
    const t = this.db
      .prepare(
        `SELECT t.id,t.goal,t.objective,t.cwd,t.lead,t.created,t.permission_profile,
      t.agent_routes,t.allow_lead_routing,j.state FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL WHERE t.id=?`
      )
      .get(ticket);
    expect(t, '工单不存在');
    t.agent_routes = JSON.parse(t.agent_routes);
    const {totalJobs,runningPeers}=this.db.prepare(`SELECT count(*) totalJobs,
      count(*) FILTER (WHERE parent IS NOT NULL AND state='running') runningPeers FROM jobs WHERE ticket=?`).get(ticket);
    const jobs = this.db
      .prepare(
        JOB_DETAILS +
          ` WHERE ticket=? ORDER BY (parent IS NULL) DESC,
      (state IN ('running','waiting','queued','interrupted')) DESC,j.id DESC LIMIT 80`
      )
      .all(ticket)
      .map(jobView);
    const kinds = VISIBLE.map(() => '?').join(',');
    // Work only on visible events. Project reply text before crossing IPC.
    const rows = this.db
      .prepare(
        `SELECT seq,kind,ts,job,turn,CASE
      WHEN kind='reply' THEN json_object('text',substr(json_extract(payload,'$.text'),1,64000))
      WHEN kind='mcp.result' THEN json_object('id',json_extract(payload,'$.id'),'server',json_extract(payload,'$.server'),
        'tool',json_extract(payload,'$.tool'),'state',json_extract(payload,'$.state'))
      WHEN kind IN ('user','user.follow','delta','failed','cancelled','interrupted','auto.recover') THEN
        CASE WHEN json_type(payload)='text' THEN json_quote(substr(json_extract(payload,'$'),1,64000)) ELSE payload END
      ELSE payload END payload FROM events WHERE ticket=? AND seq>? AND seq<=? AND kind IN (${kinds})
      ORDER BY seq ${after ? 'ASC' : 'DESC'} LIMIT 180`
      )
      .all(ticket, after, end, ...VISIBLE);
    const extra=this.progress.read(ticket,after,end,!after,180,VISIBLE);
    const merged=[...rows.map(r=>({...r,payload:JSON.parse(r.payload)})),...extra].sort((a,b)=>a.seq-b.seq);
    const visible=after?merged.slice(0,180):merged.slice(-180);
    const cursor = after && visible.length === 180 ? visible.at(-1).seq : end;
    const owners=new Map(jobs.map(j=>[j.id,j]));
    const tasks=native.tasks.map(task=>{
      if(task.state!=='running')return task;
      if(!owners.has(task.job))owners.set(task.job,this.db.prepare('SELECT state,turn FROM jobs WHERE id=? AND ticket=?').get(task.job,ticket));
      const owner=owners.get(task.job);
      return owner?.state==='running'&&owner.turn===task.turn?task:{...task,state:'unknown'};
    });
    const approvals = this.db
      .prepare("SELECT id,job,action FROM approvals WHERE ticket=? AND decision='pending' ORDER BY id")
      .all(ticket);
    return {
      ticket: t,
      jobs,
      totalJobs,
      runningPeers,
      nativeActivity:{tasks,latest:native.latest},
      events: visible,
      cursor,
      approvals: approvals.map((r) => ({ ...r, action: JSON.parse(r.action) })),
      run:
        this.db.prepare('SELECT phase,milestone,learning FROM continuous_runs WHERE ticket=?').get(ticket) ||
        null,
      extensions: this.engine.extensions?.summary(ticket),
      plan: this.db.prepare('SELECT id,body,done FROM studio_plan WHERE ticket=? ORDER BY id').all(ticket)
    };
  }
  defaults() {
    return JSON.parse(
      this.db.prepare('SELECT routes FROM route_defaults WHERE singleton=1').get()?.routes || '{}'
    );
  }
  saveDefaults({ selections }) {
    this.db
      .prepare('INSERT OR REPLACE INTO route_defaults(singleton,routes) VALUES(1,?)')
      .run(JSON.stringify(parseRoutes(selections)));
  }
  saveRoutes({ ticket, selections }) {
    id(ticket);
    parseRoutes(selections);
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const row = this.db.prepare('SELECT agent_routes FROM tickets WHERE id=?').get(ticket);
      expect(row, '工单不存在');
      expect(
        !this.db
          .prepare(
            "SELECT 1 FROM jobs WHERE ticket=? AND state NOT IN ('cancelled','failed','partial','done')"
          )
          .get(ticket),
        '请先停止或完成工单'
      );
      const old = JSON.parse(row.agent_routes),
        changed = AGENTS.filter((k) =>
          ['provider', 'model', 'mode'].some((f) => old[k]?.[f] !== selections[k]?.[f])
        );
      for (const agent of changed) {
        this.db
          .prepare('DELETE FROM native_routes WHERE job IN (SELECT id FROM jobs WHERE ticket=? AND agent=?)')
          .run(ticket, agent);
        this.db
          .prepare('UPDATE jobs SET session=NULL,route_override=NULL WHERE ticket=? AND agent=?')
          .run(ticket, agent);
      }
      this.db.prepare('UPDATE tickets SET agent_routes=? WHERE id=?').run(JSON.stringify(selections), ticket);
      if (changed.length) {
        const root = this.db
          .prepare('SELECT id,turn FROM jobs WHERE ticket=? AND parent IS NULL')
          .get(ticket);
        const payload = JSON.stringify({ old, new: selections, new_native_session: changed });
        this.db
          .prepare(
            "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?, 'routes.changed',?,?)"
          )
          .run(
            Date.now() / 1000,
            ticket,
            root.id,
            root.turn,
            payload,
            createHash('sha256').update(payload).digest('hex')
          );
      }
      this.db.exec('COMMIT');
    } catch (e) {
      this.db.exec('ROLLBACK');
      throw e;
    }
  }
  catalog({ agent }) {
    expect(AGENTS.includes(agent), '未知执行器');
    return this.adapters.catalog(agent);
  }
  catalogRevision({ agent }) {
    expect(AGENTS.includes(agent), '未知执行器');
    return this.adapters.catalogRevision(agent);
  }
  async create(p) {
    const goal = text(p.goal);
    expect(goal.trim(), '请输入目标');
    const cwd = p.cwd
      ? this.project(p.cwd)
      : this.authorize(await fsp.mkdtemp(path.join(os.tmpdir(), 'super-agent-project-')));
    expect(AGENTS.includes(p.lead), '请选择主控');
    expect(['readonly', 'ask', 'partial', 'full', 'auto'].includes(p.permission_profile), '无效权限');
    return this.engine.create({
      goal,
      cwd,
      lead: p.lead,
      permission_profile: p.permission_profile,
      selections: parseRoutes(p.selections ?? this.defaults()),
      allow_lead_routing: p.allow_lead_routing !== false,
      files: p.files || [],
      extensions: p.extensions
    });
  }
  extensions({ cwd }) {
    return this.engine.extensions.list(cwd ? this.project(cwd) : undefined);
  }
  extensionDefaults({ skills, servers, cwd }) {
    return this.engine.extensions.setDefaults({ skills, servers }, cwd ? this.project(cwd) : undefined);
  }
  mcpSave(row) {
    return this.engine.extensions.saveServer(row);
  }
  mcpRemove({ id }) {
    return this.engine.extensions.remove(id);
  }
  mcpProbe({ id }) {
    return this.engine.extensions.probe(id);
  }
  registerSkill({ folder }) {
    return this.engine.extensions.import(folder);
  }
  forgetSkill({ id }) {
    return this.engine.extensions.forget(id);
  }
  skillPreview({ id, cwd }) {
    return this.engine.extensions.preview(id, cwd ? this.project(cwd) : undefined);
  }
  files({ cwd, q = '' }) {
    return this.native.call('files', { cwd: this.project(cwd), q: text(q, 1024) });
  }
  async diff({ cwd }) {
    const root = this.project(cwd);
    const [status, staged, changes] = await Promise.all([
      git(root, ['status', '--short', '--']),
      git(root, ['diff', '--cached', '--no-ext-diff', '--no-textconv', '--']),
      git(root, ['diff', '--no-ext-diff', '--no-textconv', '--'])
    ]);
    return {
      text:
        [status && '工作区状态\n' + status, staged && '已暂存\n' + staged, changes && '未暂存\n' + changes]
          .filter(Boolean)
          .join('\n') || '工作区干净'
    };
  }
  planAdd({ ticket, body }) {
    text(body, 2000);
    expect(body.trim(), '计划项不能为空');
    this.engine.tx(()=>{
      const job=this.engine.root(id(ticket));
      const row=this.db.prepare('INSERT INTO studio_plan(ticket,body) VALUES(?,?)').run(ticket,body.trim());
      this.engine.log(job,'plan.add',{id:Number(row.lastInsertRowid)});
    });
  }
  planToggle({ ticket, id: row }) {
    this.engine.tx(()=>{
      const job=this.engine.root(id(ticket));
      const item=this.db.prepare('UPDATE studio_plan SET done=1-done WHERE ticket=? AND id=? RETURNING done').get(ticket,id(row));
      expect(item,'计划项不存在');
      this.engine.log(job,item.done?'plan.done':'plan.reopen',{id:row});
    });
  }
  async exportTo({ ticket, filename }) {
    id(ticket);
    const db = new DatabaseSync(path.join(this.dataDir, 'hub.sqlite3'), { readOnly: true });
    let file, stream;
    try {
      db.exec('BEGIN');
      expect(db.prepare('SELECT 1 FROM tickets WHERE id=?').get(ticket), '工单不存在');
      file = await fsp.open(filename, 'wx');
      stream = file.createWriteStream();
      stream.on('error', () => {});
      for (const row of db.prepare('SELECT * FROM events WHERE ticket=? ORDER BY seq').iterate(ticket)) {
        expect(
          createHash('sha256').update(row.payload).digest('hex') === row.sha256,
          '事件摘要校验失败 #' + row.seq
        );
        if (!stream.write(JSON.stringify(row) + '\n')) await once(stream, 'drain');
      }
      stream.end();
      await finished(stream);
      return filename;
    } catch (error) {
      if (file) {
        stream?.destroy();
        await file.close().catch(() => {});
        await fsp.unlink(filename).catch(() => {});
      }
      throw error;
    } finally {
      db.close();
    }
  }
}
module.exports = { Studio, AGENTS };
