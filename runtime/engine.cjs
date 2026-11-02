'use strict';

const path = require('node:path');
const { setTimeout: sleep } = require('node:timers/promises');
const { Journal, endSQL, now } = require('./engine-store.cjs');
const { command } = require('./engine-dispatch.cjs');
const P = require('./engine-protocol.cjs');
const GUIDE = require('./engine-guides.cjs');
// Yield to I/O after each admission slice, regardless of how many peers are already running.
const READS_PER_SLICE = 8;

class Engine extends Journal {
  constructor(dataDir, adapters, native) {
    super(dataDir);
    this.adapters = adapters || require('./adapters/index.cjs');
    this._native = native || null;
    this.ownsNative = !native;
    this.active = new Map();
    this.timer = null;
    this.started = false;
    this.stopping = false;
    this.fault = '';
    this.nextSlice = null;
    this.catalogRequests = new Map();
    this.writer = null;
    this.timeoutMs = 30 * 60 * 1000;
  }
  get native() {
    if (!this._native) this._native = new (require('./native.cjs').Native)();
    return this._native;
  }
  start() {
    if (this.closed || this.stopping) throw new Error('引擎正在关闭');
    if (this.started) return this;
    this.recover(); // Caller must already own the exclusive process lock.
    this.started = true;
    this.timer = setInterval(() => this.tick(), 80);
    return this;
  }
  close() {
    if (this.closePromise) return this.closePromise;
    this.stopping = true;
    clearInterval(this.timer);
    clearImmediate(this.nextSlice);
    this.closePromise = (async () => {
      for (const ctx of this.active.values()) ctx.controller.abort();
      await Promise.allSettled([...this.active.values()].map((ctx) => ctx.promise));
      if (this.ownsNative && this._native) await this._native.close();
      this.statements.clear();
      this.db.close();
      this.closed = true;
    })();
    return this.closePromise;
  }
  tick() {
    try {
      this.step();
    } catch (err) {
      this.fault = String(err.message || err);
      clearInterval(this.timer);
    }
  }
  cancel({ ticket }) {
    this.tx(() => {
      this.root(ticket);
      this.run("UPDATE approvals SET decision='expired' WHERE ticket=? AND decision='pending'", ticket);
      for (const job of this.all('SELECT * FROM jobs WHERE ticket=? AND state NOT IN ' + endSQL, ticket)) {
        this.run("UPDATE jobs SET state='cancelled' WHERE id=?", job.id);
        this.log(job, 'cancelled', '用户停止了工单');
      }
    });
    for (const ctx of this.active.values()) if (ctx.job.ticket === ticket) ctx.controller.abort();
  }
  decide({ id, allow }) {
    if (!Number.isSafeInteger(id) || typeof allow !== 'boolean') throw new Error('审批决定无效');
    this.tx(() => {
      const row = this.one(
        `SELECT j.*,a.turn AS requested FROM approvals a JOIN jobs j ON j.id=a.job AND j.ticket=a.ticket
        WHERE a.id=? AND a.decision='pending'`,
        id
      );
      if (!row || row.state !== 'running' || row.turn !== row.requested) throw new Error('审批已失效');
      const decision = allow ? 'allow' : 'deny';
      this.run('UPDATE approvals SET decision=? WHERE id=?', decision, id);
      this.log(row, 'approval.decision', { approval: id, decision });
    });
  }
  async approve(ctx, action) {
    const { job, signal } = ctx;
    if (signal.aborted || !['ask', 'partial'].includes(job.profile)) return false;
    const id = this.tx(() => {
      if (!this.live(job)) return null;
      if (job.ticket_profile === 'auto') {
        this.log(job, 'approval.unattended_decline', {
          action,
          reason: '自动工单不会代替用户批准受限子任务。'
        });
        return null;
      }
      const id = Number(
        this.run(
          'INSERT INTO approvals(ticket,job,turn,action) VALUES(?,?,?,?)',
          job.ticket,
          job.id,
          job.turn,
          JSON.stringify(action)
        ).lastInsertRowid
      );
      this.log(job, 'approval.request', { approval: id, action });
      return id;
    });
    if (!id) return false;
    while (!signal.aborted && !this.closed && this.live(job)) {
      const row = this.one('SELECT decision FROM approvals WHERE id=?', id);
      if (row?.decision !== 'pending') return row?.decision === 'allow';
      try {
        await sleep(100, undefined, { signal });
      } catch {
        break;
      }
    }
    if (!this.closed)
      this.run("UPDATE approvals SET decision='expired' WHERE id=? AND decision='pending'", id);
    return false;
  }
  wake() {
    const settled = 'NOT EXISTS(SELECT 1 FROM jobs c WHERE c.parent=p.id AND c.state NOT IN ' + endSQL + ')';
    this.run(
      "UPDATE jobs AS p SET state='queued',turn=turn+1 WHERE state='waiting' AND (" +
        settled +
        ' OR EXISTS(SELECT 1 FROM mailbox m WHERE m.target=p.id AND m.consumed_turn IS NULL))'
    );
    for (const j of this.all("SELECT p.* FROM jobs p WHERE state='draining' AND " + settled)) {
      this.run("UPDATE jobs SET state='failed' WHERE id=?", j.id);
      this.log(j, 'failed', j.result);
    }
  }
  step() {
    if (!this.started || this.stopping || this.fault) return;
    const admitted = this.tx(() => {
      this.wake();
      const slots = [];
      const queued = `SELECT j.*,t.cwd,t.claude_route,t.follow_config,t.agent_routes,t.allow_lead_routing,t.writes,
        t.permission_profile AS ticket_profile,coalesce(j.permission_override,t.permission_profile) AS profile,
        t.auto_cycles,r.route AS bound_route,c.ticket AS continuous_ticket,coalesce(c.boundary_job,0) AS floor
        FROM jobs j JOIN tickets t ON t.id=j.ticket LEFT JOIN native_routes r ON r.job=j.id
        LEFT JOIN continuous_runs c ON c.ticket=j.ticket WHERE j.state='queued' AND j.ready_after<=?
        AND j.writing=? ORDER BY j.id LIMIT ?`;
      const time = now();
      const candidates = this.all(queued, time, 0, READS_PER_SLICE);
      if (this.writer === null) candidates.push(...this.all(queued, time, 1, 1));
      candidates.sort((a, b) => a.id - b.id);
      for (const job of candidates) {
        if (this.active.has(job.id)) continue;
        if (job.profile === 'auto' && job.parent === null) {
          if (!job.continuous_ticket && job.auto_cycles >= 64) {
            const pending = this.one(
              'SELECT 1 FROM jobs WHERE parent=? AND state NOT IN ' + endSQL + ' LIMIT 1',
              job.id
            );
            const state = pending ? 'draining' : 'failed',
              reason = '旧版自动工单达到运行预算，请追加目标继续。';
            this.run('UPDATE jobs SET state=?,result=? WHERE id=?', state, reason, job.id);
            this.log(job, state, reason);
            continue;
          }
          this.run('UPDATE tickets SET auto_cycles=auto_cycles+1 WHERE id=?', job.ticket);
        }
        const packet = this.envelope(job);
        if (this.one("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_plan'"))
          packet.plan = this.all(
            'SELECT id,body,done FROM studio_plan WHERE ticket=? ORDER BY id',
            job.ticket
          );
        this.run("UPDATE jobs SET state='running' WHERE id=?", job.id);
        job.admission = this.log(job, 'turn.admitted', {});
        this.run(
          'UPDATE mailbox SET consumed_turn=? WHERE target=? AND consumed_turn IS NULL',
          job.turn,
          job.id
        );
        slots.push({ job, packet, catalog: {}, controller: new AbortController() });
      }
      return slots;
    });
    for (const ctx of admitted) {
      this.active.set(ctx.job.id, ctx);
      if (ctx.job.writing) this.writer = ctx.job.id;
      ctx.signal = ctx.controller.signal;
      ctx.promise = Promise.resolve()
        .then(() => this.execute(ctx))
        .catch((err) => {
          this.fault = '无法保存任务状态：' + String(err.message || err);
        })
        .finally(() => {
          this.active.delete(ctx.job.id);
          if (this.writer === ctx.job.id) this.writer = null;
        });
    }
    if (!this.nextSlice && admitted.length >= READS_PER_SLICE) {
      this.nextSlice = setImmediate(() => {
        this.nextSlice = null;
        this.tick();
      });
    }
  }
  catalogRequest(name) {
    if (!this.catalogRequests.has(name)) {
      const request = Promise.resolve()
        .then(() => this.adapters.catalog(name))
        .finally(() => this.catalogRequests.delete(name));
      this.catalogRequests.set(name, request);
    }
    return this.catalogRequests.get(name);
  }
  ensureCurrent(ctx) {
    if (ctx.signal.aborted || !this.live(ctx.job)) throw new Error('任务已中止');
  }
  async executionRoute(ctx) {
    const { job, packet } = ctx;
    const assigned = JSON.parse(job.agent_routes);
    let selected = JSON.parse(job.bound_route || 'null') || packet.selected_execution;
    const current = new Map();
    const lookup = (name) => {
      if (!current.has(name)) current.set(name, this.catalogRequest(name));
      return current.get(name);
    };
    const pick = (name, cat) =>
      assigned[name] ||
      (name === 'claude' && job.follow_config !== 1
        ? cat?.legacy_defaults?.[job.claude_route]
        : cat?.default);
    if (packet.allow_lead_routing) {
      await Promise.all(
        P.AGENTS.map(async (name) => {
          try {
            const cat = await lookup(name);
            const menu = P.options(cat, pick(name, cat));
            if (menu) ctx.catalog[name] = menu;
          } catch {
            /* An unavailable local provider cannot grant delegation rights. */
          }
        })
      );
    }
    this.ensureCurrent(ctx);
    packet.delegate_models = ctx.catalog;
    if (!selected) {
      const cat = await lookup(job.agent);
      selected = P.route(pick(job.agent, cat));
    } else selected = P.route(selected);
    // Existing native conversations stay on their bound route. A missing model
    // requires an explicit choice; never substitute a different upstream.
    const cat = await lookup(job.agent);
    if (!P.options(cat, selected)) throw new Error('所选供应商、模型或执行端当前不可用');
    this.ensureCurrent(ctx);
    this.tx(() => {
      this.run('INSERT OR REPLACE INTO native_routes VALUES(?,?)', job.id, JSON.stringify(selected));
      this.log(job, 'route.requested', selected);
    });
    return selected;
  }
  async execute(ctx) {
    const { job, signal } = ctx;
    const primary = !!job.continuous_ticket && job.parent === null;
    const reports = ctx.packet.reports;
    let queue = Promise.resolve(),
      pending = '',
      fatal = null;
    const invalid = new Map();
    const timer = setTimeout(() => {
      ctx.timedOut = true;
      ctx.controller.abort();
    }, this.timeoutMs);
    const submit = (line, streaming) => {
      if (!/^SA_(SEND|NOTE|ROLE|TOOL) /.test(line)) return;
      if (streaming && line.startsWith('SA_TOOL ')) return; // external effects require the authoritative final reply
      queue = queue.then(async () => {
        if (fatal || signal.aborted) return;
        try {
          await command(this, ctx, line.replace(/\r$/, ''));
        } catch (err) {
          const tag = line.split(' ', 1)[0];
          if (streaming && err instanceof SyntaxError) this.log(job, 'protocol.preview_invalid', { tag });
          else if (primary) {
            invalid.set(tag, err.message);
            this.log(job, 'auto.protocol', { tag, reason: err.message });
          } else fatal = err;
        }
      });
    };
    const projectEvent = (kind, data) => {
      if (kind === 'message.boundary') {
        if (pending) submit(pending, true);
        pending = '';
      }
      if (kind === 'delta' && typeof data === 'string') {
        const lines = (pending + data).split('\n');
        pending = lines.pop();
        for (const line of lines) submit(line, true);
      }
    };
    // Providers may project several records from a single incoming frame.
    // Acknowledgement happens only after the entire group is durably committed.
    const batch = (events) => {
      if (!events.length || signal.aborted || !this.live(job)) return;
      this.tx(() => {
        for (const [kind, data] of events) {
          const seq = this.log(job, kind, data);
          const proof = kind === 'native' && job.continuous_ticket && P.toolsProof(data);
          if (proof)
            this.run(
              `UPDATE continuous_runs SET probe=?,success=CASE WHEN ?='ok' THEN ? ELSE success END
            WHERE ticket=? AND phase='discover' AND boundary_seq<?`,
              seq,
              proof,
              seq,
              job.ticket,
              job.admission
            );
        }
      });
      for (const [kind, data] of events) projectEvent(kind, data);
    };
    const emit = (kind, data) => batch([[kind, data]]);
    emit.batch = batch;
    try {
      this.ensureCurrent(ctx);
      const selection = await this.executionRoute(ctx);
      let guide =
        GUIDE.base +
        (primary
          ? '\n' + GUIDE.continuous
          : job.profile === 'auto' && !job.continuous_ticket
            ? '\n' + GUIDE.legacyAuto
            : '');
      if (primary && ctx.packet.autonomy?.phase === 'discover') {
        guide +=
          '\nThis is a discovery turn. Inspect project evidence now. If there is new work, submit SA_GOAL with evidence AND SA_NEXT and return immediately. Wait for the work phase before implementing that task or recording its milestone. SA_FINISH is not a discovery transition.';
      }
      let packet = ctx.packet;
      if (this.extensions) {
        const ext = this.extensions.context(job.ticket, job.profile) || {};
        const allow = job.parent === null && !['legacy', 'readonly'].includes(job.profile);
        packet.project_extensions = { skills: ext.skills || [], mcp: allow ? ext.mcp || [] : [] };
        packet.mcp_results = job.parent === null ? this.extensions.results?.(job.id) || [] : [];
        if (packet.project_extensions.mcp.length)
          guide +=
            '\nUse only the listed project MCP tools. Request one tool per plain-text line: SA_TOOL {"id":"unique-per-turn","server":"listed server","tool":"listed tool","arguments":{}}. The next turn receives its durable receipt in mcp_results. Do not retry a side effect whose outcome is uncertain.';
      }
      let prompt = guide + '\n' + JSON.stringify(packet);
      if (Buffer.byteLength(prompt) > 6 * 1024 * 1024) {
        packet = await this.native.call('compact', {
          packet,
          guide,
          directory: this.dataDir,
          max_bytes: 6 * 1024 * 1024
        });
        prompt = guide + '\n' + JSON.stringify(packet);
      }
      this.ensureCurrent(ctx);
      this.tx(() => {
        this.log(job, 'input', packet);
        this.log(job, 'running', { guide, prompt_sha256: P.digest(prompt) });
      });
      if (Buffer.byteLength(prompt) > 6 * 1024 * 1024) throw new Error('输入超过 6 MiB 上限，请拆分任务');
      if (job.writing && (!P.canWrite(job.agent, job.profile) || job.profile === 'readonly'))
        throw new Error('该任务缺少写权限');
      if (!P.PROFILES.includes(job.profile) && job.profile !== 'legacy') throw new Error('未知的权限档');
      const result = await this.adapters.run(job.agent, {
        prompt,
        cwd: job.cwd,
        selection,
        profile: job.profile === 'legacy' ? (job.writing ? 'partial' : 'readonly') : job.profile,
        writable: P.isFull(job.profile) || !!job.writing,
        sessionId: job.session,
        signal,
        emit,
        approve: (action) => this.approve(ctx, action),
        pendingNotes: () =>
          signal.aborted
            ? []
            : this.notes(job.id, true).map(({ seq, sender, body }) => ({ seq, sender, body })),
        acceptNotes: (seqs) => {
          if (!signal.aborted) this.accept(job, seqs);
        }
      });
      this.ensureCurrent(ctx);
      if (!P.validText(result?.text)) throw new Error('适配器未返回有效正文');
      for (const line of result.text.split('\n')) submit(line, false);
      await queue;
      if (fatal) throw fatal;
      this.ensureCurrent(ctx);
      this.tx(() => this.commitReply(ctx, result, reports, primary, invalid));
    } catch (err) {
      // Commands already streamed can have durable receipts. Finish scheduling
      // them before classifying a broken transport and reconciling a retry.
      await queue.catch(() => {});
      this.tx(() => this.fail(ctx, err));
    } finally {
      clearTimeout(timer);
      this.run(
        "UPDATE approvals SET decision='expired' WHERE job=? AND turn=? AND decision='pending'",
        job.id,
        job.turn
      );
    }
  }
  commitReply(ctx, result, reports, primary, invalid) {
    const { job } = ctx;
    if (!this.live(job)) return;
    const seen = new Map(reports.map((r) => [r.id, r]));
    const children = this.all('SELECT id,state,result FROM jobs WHERE parent=? AND id>?', job.id, job.floor);
    const outstanding = children.some(
      (c) =>
        !P.TERMINAL.includes(c.state) ||
        seen.get(c.id)?.state !== c.state ||
        seen.get(c.id)?.result !== c.result
    );
    const mail = this.one('SELECT 1 FROM mailbox WHERE target=? AND consumed_turn IS NULL LIMIT 1', job.id);
    const follow =
      job.parent === null &&
      this.one(
        "SELECT 1 FROM events WHERE ticket=? AND kind='user.follow' AND seq>? LIMIT 1",
        job.ticket,
        job.admission
      );
    let state = outstanding || mail || follow || ctx.toolFollow ? 'waiting' : 'done',
      session = result.sessionId ?? null;
    if (job.profile === 'auto' && job.parent === null && !follow) {
      let actions = {},
        correction = null;
      try {
        if (invalid.size) throw new Error([...invalid.values()].join('；'));
        actions = P.controls(result.text, primary);
      } catch (err) {
        correction = '请修正控制消息：' + err.message;
      }
      if (primary) {
        const run = this.one('SELECT * FROM continuous_runs WHERE ticket=?', job.ticket);
        const decision = P.advance(this, job, run, actions, reports, state === 'done', correction);
        state = decision.state;
        if (decision.reset) session = null;
      } else {
        if (actions.SA_GOAL) {
          this.run('UPDATE tickets SET objective=? WHERE id=?', actions.SA_GOAL.objective, job.ticket);
          this.log(job, 'auto.goal', actions.SA_GOAL);
        }
        const finish = actions.SA_FINISH;
        if (finish && state === 'done') {
          if (finish.status === 'done' && P.unresolved(reports, finish).length) {
            state = 'waiting';
            this.log(job, 'auto.next', { body: '请核对失败的子任务并记录证据。' });
          } else {
            this.log(job, 'auto.finish', finish);
            if (finish.status === 'blocked') state = 'failed';
          }
        } else if (!finish) {
          state = 'waiting';
          this.log(
            job,
            'auto.next',
            actions.SA_NEXT || { body: correction || '请核对状态，继续或明确结束工单。' }
          );
        }
      }
    }
    if (
      state === 'done' &&
      !(job.profile === 'auto' && job.parent === null) &&
      children.some((c) => ['partial', 'failed', 'cancelled'].includes(c.state))
    )
      state = 'partial';
    const redirect = job.profile === 'auto' && !!follow;
    if (redirect) state = 'queued';
    this.run(
      'UPDATE jobs SET result=?,session=?,state=?,dispatch_turn=NULL,turn=turn+? WHERE id=?',
      result.text,
      session,
      state,
      +redirect,
      job.id
    );
    this.log(job, 'reply', { text: result.text, sha256: P.digest(result.text), session, state });
  }
  fail(ctx, err) {
    const { job } = ctx;
    if (!this.live(job)) return;
    const reason = ctx.timedOut ? '任务超时，重试前须核对实际副作用。' : String(err?.message || err);
    if (this.stopping) {
      this.interrupt(job);
      return;
    }
    const primary = job.profile === 'auto' && job.parent === null;
    if (
      primary &&
      job.continuous_ticket &&
      this.one(
        "SELECT 1 FROM events WHERE ticket=? AND kind='user.follow' AND seq>? LIMIT 1",
        job.ticket,
        job.admission
      )
    ) {
      this.run(
        "UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL,ready_after=0,result=? WHERE id=?",
        reason,
        job.id
      );
      this.log(job, 'auto.retry', { after_seconds: 0, reason: '按新追加的要求重新核对。' });
      return;
    }
    if (
      primary &&
      (ctx.timedOut ||
        /HTTP (429|50[0234])|connection reset|timed out|rate limit|service unavailable|stream disconnected|暂时不可用|ECONNRESET|ETIMEDOUT/i.test(
          reason
        ))
    ) {
      const attempts = this.one(
        `SELECT count(*) AS n FROM events WHERE job=? AND kind='auto.retry' AND seq>
        coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow')),0)`,
        job.id,
        job.id
      ).n;
      if (job.continuous_ticket || attempts < 2) {
        const delay = Math.min(300, 2 ** Math.min(attempts + 1, 9));
        this.interrupt(job);
        this.run('UPDATE jobs SET ready_after=? WHERE id=?', now() + delay, job.id);
        this.log(job, 'auto.retry', { after_seconds: delay, reason });
        return;
      }
    }
    const pending = this.one(
      'SELECT 1 FROM jobs WHERE parent=? AND state NOT IN ' + endSQL + ' LIMIT 1',
      job.id
    );
    const state = pending ? 'draining' : 'failed';
    this.run('UPDATE jobs SET state=?,result=? WHERE id=?', state, reason, job.id);
    this.log(job, state, reason);
  }
}
module.exports = { Engine };
