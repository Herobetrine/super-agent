'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { openDB, transaction } = require('./engine-schema.cjs');
const P = require('./engine-protocol.cjs');
const endSQL = "('done','partial','failed','cancelled')";
const INBOX = ['queued', 'running', 'waiting'];
const now = () => Date.now() / 1000;

class Journal {
  constructor(directory) {
    this.dataDir = path.resolve(directory);
    this.db = openDB(this.dataDir);
    this.statements = new Map();
    this.closed = false;
  }
  stmt(sql) {
    if (this.closed) throw new Error('调度台已关闭');
    if (!this.statements.has(sql)) this.statements.set(sql, this.db.prepare(sql));
    return this.statements.get(sql);
  }
  one(sql, ...args) {
    return this.stmt(sql).get(...args);
  }
  all(sql, ...args) {
    return this.stmt(sql).all(...args);
  }
  run(sql, ...args) {
    return this.stmt(sql).run(...args);
  }
  tx(fn) {
    return transaction(this.db, fn);
  }
  log(job, kind, data) {
    const payload = JSON.stringify(data ?? null);
    return Number(
      this.run(
        `INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?,?,?,?)`,
        now(),
        job.ticket,
        job.id,
        job.turn,
        kind,
        payload,
        P.digest(payload)
      ).lastInsertRowid
    );
  }
  live(job) {
    return !!this.one(
      "SELECT 1 FROM jobs WHERE id=? AND ticket=? AND turn=? AND state='running'",
      job.id,
      job.ticket,
      job.turn
    );
  }
  root(ticket) {
    const row = this.one('SELECT * FROM jobs WHERE ticket=? AND parent IS NULL', ticket);
    if (!row) throw new Error('工单不存在');
    return row;
  }
  interrupt(job) {
    const reason = '上次执行中断，可能已经产生副作用，继续前请核对实际状态。';
    if (job.ticket_profile === 'auto') {
      this.run(
        "UPDATE jobs SET state='queued',dispatch_turn=coalesce(dispatch_turn,turn),turn=turn+1,result=? WHERE id=?",
        reason,
        job.id
      );
      this.log(job, 'auto.recover', reason);
    } else {
      this.run("UPDATE jobs SET state='interrupted',result=? WHERE id=?", reason, job.id);
      this.log(job, 'interrupted', reason);
    }
  }
  recover() {
    this.tx(() => {
      this.run("UPDATE approvals SET decision='expired' WHERE decision='pending'");
      for (const j of this.all(`SELECT j.*,t.permission_profile AS ticket_profile FROM jobs j
        JOIN tickets t ON j.ticket=t.id WHERE j.state='running'`))
        this.interrupt(j);
    });
  }
  async create(params) {
    const {
      goal,
      cwd,
      lead = 'deepseek',
      permission_profile: profile = 'readonly',
      allow_lead_routing: routing = true,
      files = []
    } = params;
    if (
      !P.validText(goal) ||
      !P.validText(cwd) ||
      !P.AGENTS.includes(lead) ||
      !P.PROFILES.includes(profile) ||
      typeof routing !== 'boolean'
    )
      throw new Error('工单的目标、目录、主控或权限无效');
    const dir = fs.realpathSync(cwd);
    if (!fs.statSync(dir).isDirectory()) throw new Error('工作目录无效');
    if (params.extensions && !this.extensions) throw new Error('当前未配置项目扩展服务');
    const extensions = await this.extensions?.prepare(params.extensions, dir);
    const manifest = await this.snapshot(dir, files);
    return this.tx(() => {
      const defaults = this.one('SELECT routes FROM route_defaults WHERE singleton=1');
      const picked = P.routes(params.selections ?? JSON.parse(defaults?.routes || '{}'));
      const writes = profile !== 'readonly';
      const ticket = Number(
        this.run(
          `INSERT INTO tickets(goal,cwd,writes,lead,created,claude_route,agent_routes,
        follow_config,permission_profile,allow_lead_routing) VALUES(?,?,?,?,?,'user',?,1,?,?)`,
          goal,
          dir,
          +writes,
          lead,
          now(),
          JSON.stringify(picked),
          profile,
          +routing
        ).lastInsertRowid
      );
      this.extensions?.bind(ticket, extensions);
      if (profile === 'auto') this.run('INSERT INTO continuous_runs(ticket) VALUES(?)', ticket);
      const id = Number(
        this.run(
          `INSERT INTO jobs(ticket,sender,agent,body,writing,depth,state)
        VALUES(?,'user',?,?,?,0,'queued')`,
          ticket,
          lead,
          goal,
          +(writes && P.canWrite(lead, profile))
        ).lastInsertRowid
      );
      if (manifest.length) this.run('INSERT INTO job_files VALUES(?,?)', id, JSON.stringify(manifest));
      const job = { ticket, id, turn: 1 };
      this.log(job, 'user', goal);
      if (manifest.length) this.log(job, 'files', manifest);
      return ticket;
    });
  }
  async snapshot(cwd, names) {
    if (
      !Array.isArray(names) ||
      names.length > 16 ||
      names.some(
        (n) =>
          !P.validText(n) || path.isAbsolute(n) || path.win32.parse(n).root || n.split(/[\\/]/).includes('..')
      )
    )
      throw new Error('附件需要最多 16 个项目相对路径');
    if (!names.length) return [];
    const result = await this.native.call('snapshot', {
      project: cwd,
      names,
      directory: path.join(this.dataDir, 'objects')
    });
    if (
      !Array.isArray(result) ||
      result.length !== names.length ||
      result.some(
        (f, i) =>
          f?.name !== names[i] ||
          !Number.isSafeInteger(f.bytes) ||
          f.bytes < 0 ||
          !/^[a-f0-9]{64}$/.test(f.sha256) ||
          !path.isAbsolute(f.object_path)
      )
    )
      throw new Error('快照清单无效');
    return result;
  }
  follow({ ticket, body, resume = false }) {
    if (!P.validText(body)) throw new Error('追加指令为空');
    return this.tx(() => {
      const root = this.root(ticket);
      const cont = this.one('SELECT 1 FROM continuous_runs WHERE ticket=?', ticket);
      if (!cont && ['interrupted', 'draining'].includes(root.state))
        throw new Error('请先处理上级中断或失败');
      if (cont && root.state === 'cancelled' && resume !== true) throw new Error('停止的工单需要明确恢复');
      const seq = this.log(root, 'user.follow', body);
      if (cont) {
        this.run(
          `UPDATE continuous_runs SET phase='work',probe=0,success=0,stalls=0,last_signal='',
          boundary_seq=? WHERE ticket=?`,
          seq,
          ticket
        );
        this.run('UPDATE jobs SET ready_after=0 WHERE id=?', root.id);
      }
      const auto =
        this.one('SELECT permission_profile FROM tickets WHERE id=?', ticket).permission_profile === 'auto';
      if (auto) this.run('UPDATE tickets SET auto_cycles=0,objective=? WHERE id=?', body, ticket);
      if (
        P.TERMINAL.includes(root.state) ||
        (auto && (root.state === 'waiting' || (cont && ['interrupted', 'draining'].includes(root.state))))
      )
        this.run("UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL WHERE id=?", root.id);
      return seq;
    });
  }
  retry({ job: id }) {
    this.tx(() => {
      const job = this.one('SELECT * FROM jobs WHERE id=?', id);
      if (!job || !['failed', 'interrupted'].includes(job.state)) throw new Error('只能重试失败或中断的任务');
      let parent = job.parent;
      while (parent) {
        const row = this.one('SELECT * FROM jobs WHERE id=?', parent);
        if (!row || ['running', 'cancelled', 'interrupted'].includes(row.state))
          throw new Error('请先处理上级任务');
        if (row.state !== 'draining') this.run("UPDATE jobs SET state='waiting' WHERE id=?", parent);
        parent = row.parent;
      }
      this.run("UPDATE jobs SET state='queued',ready_after=0 WHERE id=?", id);
      this.log(job, 'retry', '重试同一逻辑轮次，保留已提交消息的幂等收据。');
    });
  }
  recordNote(ticket, dest, body, source = null, key = null) {
    if (!Number.isSafeInteger(dest) || dest <= 0 || !P.validText(body)) throw new Error('收件任务或正文无效');
    const turn = source && (source.dispatch_turn || source.turn);
    if (source) {
      const old = this.one(
        `SELECT m.seq,e.payload FROM mailbox m JOIN events e ON e.seq=m.seq
        WHERE m.source=? AND m.source_turn=? AND m.msgkey=?`,
        source.id,
        turn,
        key
      );
      if (old) {
        const p = JSON.parse(old.payload);
        if (p.job !== dest || p.body !== body) throw new Error('同一 SA_NOTE ID 的内容不一致');
        return old.seq;
      }
    }
    const receiver = this.one('SELECT * FROM jobs WHERE id=? AND ticket=?', dest, ticket);
    let rejection = null;
    if (!receiver || !INBOX.includes(receiver.state)) rejection = '目标不属于本工单或已停止收信';
    else if (
      source &&
      this.one(
        `SELECT count(*) AS n FROM mailbox m JOIN jobs j ON j.id=m.target
      WHERE j.ticket=? AND m.source IS NOT NULL AND (NOT EXISTS(SELECT 1 FROM continuous_runs WHERE ticket=?)
      OR (m.consumed_turn IS NULL AND j.state NOT IN ` +
          endSQL +
          '))',
        ticket,
        ticket
      )?.n >= 128
    )
      rejection = '自动信箱达到预算';
    if (!source && rejection) throw new Error(rejection);
    const payload = {
      job: dest,
      sender: source ? source.agent + ' (#' + source.id + ')' : 'user',
      body,
      sha256: P.digest(body)
    };
    if (key) payload.id = key;
    if (rejection) payload.reason = rejection;
    const seq = this.log(source || receiver, rejection ? 'note.rejected' : 'note', payload);
    this.run(
      'INSERT INTO mailbox(seq,target,source,source_turn,msgkey) VALUES(?,?,?,?,?)',
      seq,
      rejection ? null : dest,
      source?.id ?? null,
      turn,
      key
    );
    return seq;
  }
  note({ ticket, job, body }) {
    return this.tx(() => this.recordNote(ticket, job, body));
  }
  notes(id, pending = false) {
    return this.all(
      `SELECT m.seq,e.payload FROM mailbox m JOIN events e ON m.seq=e.seq WHERE m.target=?` +
        (pending ? ' AND consumed_turn IS NULL' : '') +
        ' ORDER BY m.seq',
      id
    ).map((r) => ({ seq: r.seq, ...JSON.parse(r.payload) }));
  }
  accept(job, seqs) {
    this.tx(() => {
      if (!this.live(job)) return;
      const accepted = [];
      for (const seq of seqs)
        if (
          Number.isSafeInteger(seq) &&
          this.run(
            'UPDATE mailbox SET consumed_turn=? WHERE seq=? AND target=? AND consumed_turn IS NULL',
            job.turn,
            seq,
            job.id
          ).changes
        )
          accepted.push(seq);
      if (accepted.length) this.log(job, 'note.accepted', { seqs: accepted, via: 'native' });
    });
  }
  envelope(j) {
    const t = this.one('SELECT * FROM tickets WHERE id=?', j.ticket);
    const c = this.one('SELECT * FROM continuous_runs WHERE ticket=?', j.ticket);
    const floor = c?.boundary_job || 0;
    const reports = this.all(
      'SELECT id,agent AS sender,role,body AS assignment,state,result FROM jobs WHERE parent=? AND id>? ORDER BY id',
      j.id,
      floor
    );
    const recent = c
      ? this.all(
          `SELECT seq,kind,payload FROM events WHERE ticket=? AND kind IN ('auto.milestone','auto.discovered')
      ORDER BY seq DESC LIMIT 8`,
          j.ticket
        )
          .reverse()
          .map((r) => ({ seq: r.seq, kind: r.kind, data: JSON.parse(r.payload) }))
      : [];
    const next = this.one(
      `SELECT payload FROM events WHERE job=? AND kind='auto.next' AND seq>coalesce(
      (SELECT max(seq) FROM events WHERE ticket=? AND kind IN ('user.follow','auto.finish','auto.milestone','auto.discovered','auto.exhausted')),0)
      ORDER BY seq DESC LIMIT 1`,
      j.id,
      j.ticket
    );
    const files = this.one('SELECT manifest FROM job_files WHERE job=?', j.id);
    return {
      ticket: j.ticket,
      job: j.id,
      turn: j.turn,
      depth: j.depth,
      agent: j.agent,
      sender: j.sender,
      role: j.role,
      project: t.cwd,
      goal: t.goal,
      assignment: j.body,
      objective: t.objective || t.goal,
      previous_reply: j.result,
      reports,
      permission_profile: j.profile,
      allow_lead_routing: !!t.allow_lead_routing && j.parent === null,
      allowed_child_permissions: t.allow_lead_routing && j.parent === null ? P.childProfiles(j.profile) : [],
      can_assign_write: !!t.writes && j.profile !== 'readonly',
      this_turn_can_write: P.isFull(j.profile) || !!j.writing,
      harness_capabilities: Object.fromEntries(
        P.AGENTS.map((a) => [a, { read_project: true, write_project: P.canWrite(a, j.profile) }])
      ),
      selected_execution:
        JSON.parse(j.route_override || 'null') || JSON.parse(t.agent_routes)[j.agent] || null,
      claude_route: t.claude_route,
      files: files ? JSON.parse(files.manifest) : [],
      notes: this.notes(j.id),
      user_updates: this.all(
        "SELECT payload FROM events WHERE ticket=? AND kind='user.follow' ORDER BY seq",
        j.ticket
      ).map((r) => JSON.parse(r.payload)),
      roster: this.all(
        'SELECT id AS job,agent,state,role,parent FROM jobs WHERE ticket=? AND (parent IS NULL OR id>?) ORDER BY id',
        j.ticket,
        floor
      ),
      next_step: next ? JSON.parse(next.payload) : null,
      autonomy: c
        ? {
            phase: c.phase,
            milestone: c.milestone,
            learning: c.learning,
            recent,
            last_inspection_seq: c.probe
          }
        : null
    };
  }
}

module.exports = { Journal, endSQL, now };
