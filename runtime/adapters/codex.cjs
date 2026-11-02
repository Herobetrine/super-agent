'use strict';
const { path, capture, start, jsonLines, select, emitBatch } = require('./common.cjs');
const { discover, routing } = require('./codex-config.cjs');
function policy(profile, writable, approve) {
  const p = profile ?? 'legacy';
  if (!['legacy', 'readonly', 'ask', 'partial', 'auto', 'full'].includes(p))
    throw Error('未知 Codex 权限档位');
  if (p === 'full' || p === 'auto') {
    if (!writable) throw Error('完全访问与只读授权冲突');
    return { sandbox: 'danger-full-access', review: 'never', native: true };
  }
  if (p === 'readonly' && writable) throw Error('只读档不允许项目写入');
  const interactive = writable && (p === 'ask' || p === 'partial');
  if (interactive && typeof approve !== 'function') throw Error('本机缺少 Codex 审批通道');
  return {
    sandbox: writable && p !== 'ask' ? 'workspace-write' : 'read-only',
    review: interactive ? 'on-request' : 'never',
    native: false
  };
}
const policyTypes = {
  'read-only': 'readOnly',
  'workspace-write': 'workspaceWrite',
  'danger-full-access': 'dangerFullAccess'
};
const requests = {
  'item/commandExecution/requestApproval': '执行命令或访问网络',
  'item/fileChange/requestApproval': '修改文件',
  'item/permissions/requestApproval': '扩展本轮权限'
};
function reviewable(method, p, fileItem) {
  if (method === 'item/commandExecution/requestApproval')
    return !!(p.command || p.networkApprovalContext?.host);
  if (method === 'item/fileChange/requestApproval') return !!fileItem?.changes?.length;
  return (
    p.permissions &&
    typeof p.permissions === 'object' &&
    Object.keys(p.permissions).length > 0 &&
    Object.keys(p.permissions).every((k) => ['fileSystem', 'network'].includes(k))
  );
}
function decision(method, p, allowed) {
  return method === 'item/permissions/requestApproval'
    ? { permissions: allowed ? p.permissions : {}, scope: 'turn' }
    : { decision: allowed ? 'accept' : 'decline' };
}
function appReply(method, p, yes) {
  if (method === 'mcpServer/elicitation/request') {
    const schema = p.requestedSchema;
    const once =
      yes &&
      p.mode === 'form' &&
      p._meta?.codex_approval_kind === 'mcp_tool_call' &&
      !p._meta.codex_requires_user_input &&
      schema?.type === 'object' &&
      Object.keys(schema).length === 2 &&
      !!schema.properties &&
      Object.keys(schema.properties).length === 0;
    return once ? { action: 'accept', content: {} } : { action: 'decline', content: null };
  }
  const q = p.questions?.[0],
    labels = new Set(q?.options?.map((o) => o.label));
  const once =
    yes &&
    p.questions?.length === 1 &&
    q.id === 'mcp_tool_call_approval_' + p.itemId &&
    q.header === 'Approve app tool call?' &&
    q.isSecret === false &&
    q.isOther === false &&
    labels.has('Allow') &&
    labels.has('Cancel');
  return { answers: once ? { [q.id]: { answers: ['Allow'] } } : {} };
}
function rpcError(e) {
  const message = String(e?.message || '');
  const code = message.match(/\b(401|403|429|500|502|503|504)\b/);
  return Error(
    'Codex 原生请求失败' + (code ? '（HTTP ' + code[1] + '）' : '') + '，请核对路由、权限或网络状态'
  );
}
async function run(o) {
  const perm = policy(o.profile, o.writable, o.approve);
  const d = await discover(o.signal),
    s = select(d.catalog, o.selection),
    { args, settings } = routing(d, s);
  if (perm.review === 'on-request') {
    for (const setting of [
      'network_access=false',
      'writable_roots=[]',
      'exclude_tmpdir_env_var=true',
      'exclude_slash_tmp=true'
    ])
      args.push('-c', 'sandbox_workspace_write.' + setting);
  }
  if (!perm.native) {
    for (const f of ['apps', 'plugins', 'hooks', 'multi_agent']) args.push('--disable', f);
    const mcp = JSON.parse(
      await capture(d.exe, [...args, 'mcp', 'list', '--json'], {
        cwd: o.cwd,
        signal: o.signal,
        timeout: 25000
      })
    );
    for (const server of mcp) {
      if (!/^[\w-]+$/.test(server.name)) throw Error('MCP 名称无法安全覆盖');
      args.push('-c', 'mcp_servers.' + server.name + '.enabled=false');
    }
  }
  o.signal?.throwIfAborted();
  const proc = start(d.exe, [...args, 'app-server', '--listen', 'stdio://'], { cwd: o.cwd });
  let threadId,
    turnId,
    lastText,
    live = true,
    fault,
    steer,
    polling = false,
    pollNotes = true,
    killTimer;
  const fileItems = new Map(),
    deltaItems = new Set(),
    pending = new Map();
  const send = (msg) => {
    if (live && !proc.child.stdin.destroyed) proc.child.stdin.write(JSON.stringify(msg) + '\n');
  };
  const stopWith = (err) => {
    fault = err;
    void proc.stop();
  };
  const cancel = () => {
    if (turnId) {
      send({ id: 4, method: 'turn/interrupt', params: { threadId, turnId } });
      killTimer = setTimeout(() => void proc.stop(), 2400);
      killTimer.unref();
    } else void proc.stop();
  };
  o.signal?.addEventListener('abort', cancel, { once: true });
  if (o.signal?.aborted) cancel();
  const setup = setTimeout(() => stopWith(Error('Codex 建立轮次超时')), 90000);
  setup.unref();
  async function poll() {
    if (
      !live ||
      !pollNotes ||
      !turnId ||
      o.signal?.aborted ||
      steer ||
      polling ||
      !o.pendingNotes ||
      !o.acceptNotes
    )
      return;
    polling = true;
    try {
      const note = (await o.pendingNotes())?.[0];
      if (!note || !live || o.signal?.aborted) return;
      const text = 'Same-ticket update from the named sender:\n' + JSON.stringify(note);
      if (Buffer.byteLength(text) > 65536) {
        pollNotes = false;
        await o.emit('note.deferred', { seq: note.seq, reason: '超过实时插入大小' });
        return;
      }
      steer = { id: 'steer-' + note.seq, seq: note.seq };
      send({
        id: steer.id,
        method: 'turn/steer',
        params: { threadId, expectedTurnId: turnId, input: [{ type: 'text', text }] }
      });
    } catch {
      if (live) stopWith(Error('Codex 信箱传输失败'));
    } finally {
      polling = false;
    }
  }
  const timer = setInterval(() => void poll(), 400);
  timer.unref();
  try {
    send({
      id: 1,
      method: 'initialize',
      params: {
        clientInfo: { name: 'superagent_local', title: 'Super Agent', version: '1.0' },
        capabilities: { experimentalApi: perm.review === 'on-request' }
      }
    });
    for await (const event of jsonLines(proc.child.stdout)) {
      const { method, id } = event,
        p = event.params || {};
      if (steer && id === steer.id && !method) {
        const seq = steer.seq;
        steer = null;
        if (!event.error && event.result?.turnId === turnId) await o.acceptNotes([seq]);
        else {
          pollNotes = false;
          await o.emit('note.deferred', { seq, reason: '原生轮次未确认收件' });
        }
        continue;
      }
      if (event.error) throw rpcError(event.error);
      if (method && id != null) {
        const same =
          !!threadId && !!turnId && p.threadId === threadId && p.turnId === turnId && !o.signal?.aborted;
        if (['mcpServer/elicitation/request', 'tool/requestUserInput'].includes(method)) {
          send({ id, result: appReply(method, p, perm.native && same) });
          continue;
        }
        if (!Object.hasOwn(requests, method)) {
          send({ id, error: { code: -32601, message: 'Unsupported server request' } });
          continue;
        }
        const fileItem = fileItems.get(p.itemId);
        if (
          !same ||
          !p.itemId ||
          pending.has(id) ||
          (!perm.native && perm.review !== 'on-request') ||
          (p.availableDecisions && !p.availableDecisions.includes('accept')) ||
          !reviewable(method, p, fileItem)
        ) {
          send({ id, result: decision(method, p, false) });
          continue;
        }
        if (perm.native) {
          send({ id, result: decision(method, p, true) });
          continue;
        }
        const action = {
          adapter: 'codex',
          method,
          title: requests[method],
          request_id: id,
          params: p,
          ...(fileItem ? { item: fileItem } : {})
        };
        pending.set(id, true);
        void Promise.resolve()
          .then(() => o.approve(action))
          .catch(() => false)
          .then((allow) => {
            if (live && pending.delete(id))
              send({ id, result: decision(method, p, allow === true && !o.signal?.aborted) });
          });
        continue;
      }
      if (id === 1) {
        send({ method: 'initialized', params: {} });
        const opts = {
          cwd: path.resolve(o.cwd),
          sandbox: perm.sandbox,
          approvalPolicy: perm.review,
          model: s.model,
          modelProvider: s.provider
        };
        if (perm.review === 'on-request') opts.approvalsReviewer = 'user';
        if (Object.keys(settings).length) opts.config = settings;
        if (o.sessionId) Object.assign(opts, { threadId: o.sessionId, excludeTurns: true });
        send({ id: 2, method: o.sessionId ? 'thread/resume' : 'thread/start', params: opts });
      } else if (id === 2) {
        const r = event.result;
        if (
          r.sandbox?.type !== policyTypes[perm.sandbox] ||
          r.approvalPolicy !== perm.review ||
          (perm.review === 'on-request' &&
            (r.approvalsReviewer !== 'user' ||
              r.sandbox.networkAccess !== false ||
              r.sandbox.writableRoots?.length))
        )
          throw Error('Codex 未确认请求的原生权限');
        if (r.model !== s.model || r.modelProvider !== s.provider)
          throw Error('Codex 未采用所选供应商和模型');
        threadId = r.thread?.id;
        if (!threadId || (o.sessionId && threadId !== o.sessionId)) throw Error('Codex 会话不匹配');
        await o.emit('permission.effective', {
          profile: o.profile,
          sandbox: r.sandbox,
          approvalPolicy: r.approvalPolicy,
          reviewer: r.approvalsReviewer
        });
        await o.emit('route.selected', s);
        send({
          id: 3,
          method: 'turn/start',
          params: { threadId, input: [{ type: 'text', text: o.prompt }] }
        });
      } else if (id === 3) {
        turnId = event.result?.turn?.id;
        if (turnId) clearTimeout(setup);
      } else if (method && threadId && (p.threadId === threadId || p.thread?.id === threadId)) {
        const frame = [['native', event]];
        if (method === 'item/agentMessage/delta' && typeof p.delta === 'string') {
          frame.push(['delta', p.delta]);
          if (p.delta && p.itemId) deltaItems.add(p.itemId);
        }
        if (method === 'item/completed' && p.item.type === 'agentMessage' && deltaItems.delete(p.item.id))
          frame.push(['message.boundary', null]);
        await emitBatch(o.emit, frame);
        if (method === 'turn/started') {
          turnId = p.turn.id;
          clearTimeout(setup);
        }
        if (method === 'serverRequest/resolved') pending.delete(p.requestId);
        if (method === 'item/started' && p.item.type === 'fileChange') fileItems.set(p.item.id, p.item);
        if (method === 'item/completed') {
          if (p.item.type === 'agentMessage') {
            if (p.item.phase !== 'commentary') lastText = p.item.text;
          }
          fileItems.delete(p.item.id);
        }
        if (method === 'turn/completed' && p.turn.id === turnId) {
          o.signal?.throwIfAborted();
          if (p.turn.status !== 'completed') throw rpcError(p.turn.error);
          if (typeof lastText !== 'string') throw Error('Codex 轮次缺少终稿');
          return { text: lastText, sessionId: threadId };
        }
      }
    }
    o.signal?.throwIfAborted();
    throw fault || Error('Codex 在完成前断开了会话');
  } finally {
    live = false;
    pending.clear();
    clearInterval(timer);
    clearTimeout(setup);
    clearTimeout(killTimer);
    o.signal?.removeEventListener('abort', cancel);
    proc.child.stdin.end();
    const hard = setTimeout(() => void proc.stop(), 1800);
    hard.unref();
    await proc.finished;
    clearTimeout(hard);
    if (steer) await o.emit('note.deferred', { seq: steer.seq, reason: '通道结束前未收到回执' });
  }
}
module.exports = {
  catalog: async () => (await discover()).catalog,
  run,
  watchPaths: require('./codex-config.cjs').watchPaths
};
