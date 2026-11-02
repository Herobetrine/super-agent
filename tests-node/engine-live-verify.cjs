'use strict';

// Re-check the recorded native probes, regardless of later review jobs using
// the same harness. This verifier never invokes a provider or starts a scheduler.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { DatabaseSync } = require('node:sqlite');
const hash = (b) => crypto.createHash('sha256').update(b).digest('hex');

function verify(root, ticket = 1) {
  const project = path.join(root, 'project');
  const db = new DatabaseSync(path.join(root, 'state', 'hub.sqlite3'), { readOnly: true });
  try {
    const events = db.prepare('SELECT * FROM events WHERE ticket=? ORDER BY seq').all(ticket);
    const jobs = db.prepare('SELECT * FROM jobs WHERE ticket=? ORDER BY id').all(ticket);
    const primary = jobs.find((j) => j.parent === null);
    const of = (kind) => events.filter((e) => e.kind === kind);
    const milestone = of('auto.milestone');
    const seqLimit = milestone[0]?.seq ?? Infinity;
    const firstChildren = new Set(
      of('send')
        .filter((e) => e.seq < seqLimit && e.job === primary.id)
        .map((e) => JSON.parse(e.payload).child)
    );
    const reports = of('input')
      .filter((e) => e.job === primary.id)
      .flatMap((e) => JSON.parse(e.payload).reports);
    const peers = {};
    for (const name of ['codex', 'claude', 'antigravity']) {
      const token = fs.readFileSync(path.join(project, name + '.txt'), 'utf8');
      const candidates = jobs.filter(
        (j) => firstChildren.has(j.id) && j.agent === name && j.state === 'done'
      );
      peers[name] = { done: false, exact_file_token: false, native_to_router_exact: false };
      for (const child of candidates) {
        const raw = of('native')
          .filter((e) => e.job === child.id)
          .map((e) => JSON.parse(e.payload));
        let native = null;
        for (const a of raw) {
          if (a.type === 'result' && a.subtype === 'success') native = a.result;
          if (a.event === 'result' && a.result?.status === 'SUCCESS') native = a.result.response;
          if (
            a.method === 'item/completed' &&
            a.params?.item?.type === 'agentMessage' &&
            a.params.item.phase !== 'commentary'
          )
            native = a.params.item.text;
        }
        const exact = [token, token + '\n', token + '\r\n'].includes(native);
        const handoff =
          typeof native === 'string' &&
          native === child.result &&
          reports.some((r) => r.id === child.id && r.result === native);
        peers[name] = {
          job: child.id,
          done: true,
          exact_file_token: exact,
          native_to_router_exact: handoff,
          steps: raw.length,
          scope: child.permission_override
        };
        if (exact && handoff) break;
      }
    }
    const observed = (kind) =>
      of(kind).some((e) => {
        const payload = JSON.parse(e.payload),
          seq = payload.last_success_seq || payload.probe_seq;
        return events.some((r) => r.seq === seq && r.kind === 'native' && r.seq < e.seq);
      });
    const matches = (file, expected) =>
      fs.existsSync(path.join(project, file)) && fs.readFileSync(path.join(project, file)).equals(expected);
    const input = path.join(project, 'input.bin');
    const stop = db
      .prepare(
        'SELECT j.ticket,j.state FROM jobs j WHERE j.parent IS NULL AND j.ticket>? ORDER BY j.ticket DESC LIMIT 1'
      )
      .get(ticket);
    const out = path.join(root, 'events.jsonl');
    const archived = fs.existsSync(out)
      ? fs
          .readFileSync(out, 'utf8')
          .trimEnd()
          .split('\n')
          .map((x) => JSON.parse(x))
      : [];
    const checks = {
      terminal: primary.state === 'done',
      first: matches('delivery.txt', fs.readFileSync(path.join(project, 'source.txt'))),
      second:
        fs.existsSync(input) &&
        matches('delivery.sha256', Buffer.from(hash(fs.readFileSync(input)), 'ascii')),
      milestones: milestone.length >= 2,
      discovered: observed('auto.discovered'),
      learning: of('auto.learn').length > 0,
      exhausted: observed('auto.exhausted'),
      all_receipts:
        events.every(
          (e, i) =>
            hash(e.payload) === e.sha256 &&
            archived[i]?.seq === e.seq &&
            archived[i]?.sha256 === e.sha256 &&
            archived[i].payload === e.payload
        ) && archived.length === events.length,
      peers: Object.values(peers).every((p) => p.done && p.exact_file_token && p.native_to_router_exact),
      no_follow: of('user.follow').length === 0,
      no_approvals: of('approval.request').length === 0,
      stop:
        stop?.state === 'cancelled' &&
        !!db.prepare("SELECT 1 FROM events WHERE ticket=? AND kind='native'").get(stop.ticket)
    };
    return {
      passed: Object.values(checks).every(Boolean),
      ticket,
      rootTurn: primary.turn,
      rows: events.length,
      checks,
      peers,
      failedJobs: jobs.filter((j) => j.state === 'failed').map((j) => ({ id: j.id, agent: j.agent })),
      archive_sha256: fs.existsSync(out) ? hash(fs.readFileSync(out)) : null
    };
  } finally {
    db.close();
  }
}
module.exports = { verify };
if (require.main === module) {
  const dir = path.resolve(process.argv[2]),
    r = verify(dir);
  fs.writeFileSync(path.join(dir, 'verified-replay.json'), JSON.stringify(r, null, 2));
  console.log(JSON.stringify(r));
  if (!r.passed) process.exitCode = 1;
}
