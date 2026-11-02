'use strict';

// Opt-in: exercises paid, full-access native CLI routes in a disposable project.
// Run with node tests-node/engine-live.cjs. No external messages or publishing.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { setTimeout: sleep } = require('node:timers/promises');
const { Engine } = require('../runtime/engine.cjs');

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'superagent-node-autonomy-'));
  console.log('Local fixture:', root);
  const project = path.join(root, 'project'),
    state = path.join(root, 'state');
  fs.mkdirSync(project);
  const targets = ['codex', 'claude', 'antigravity'],
    tokens = {};
  for (const name of ['source', ...targets]) {
    tokens[name] = crypto.randomBytes(8).toString('hex');
    fs.writeFileSync(path.join(project, name + '.txt'), tokens[name]);
  }
  const secondInput = crypto.randomBytes(32);
  let tid,
    added = false,
    released = false;
  class FixtureEngine extends Engine {
    step() {
      // Observe the committed first milestone before admitting discovery. The
      // fixture changes on disk, without any second operator instruction.
      if (
        tid &&
        !added &&
        this.one("SELECT seq FROM events WHERE ticket=? AND kind='auto.milestone' LIMIT 1", tid)
      ) {
        fs.writeFileSync(
          path.join(project, 'PROJECT_INBOX.md'),
          '新的待领工单：生成校验文件 delivery.sha256。探查阶段先依据本清单提交 SA_GOAL（含 evidence）与 SA_NEXT 并立即回合结束。平台确认领取目标、切回 work 后，测试方才发放本次独立批次的 input.bin。届时将 input.bin 的 SHA-256 写作 64 位小写 ASCII，不加换行，执行独立核验并提交里程碑。该条目通过后清单全部完成。'
        );
        added = true;
      }
      if (
        tid &&
        added &&
        !released &&
        this.one("SELECT seq FROM events WHERE ticket=? AND kind='auto.discovered' LIMIT 1", tid)
      ) {
        fs.writeFileSync(path.join(project, 'input.bin'), secondInput);
        released = true;
      }
      return super.step();
    }
  }
  const e = new FixtureEngine(state);
  e.timeoutMs = 180000;
  let stopId;
  try {
    tid = await e.create({
      cwd: project,
      lead: 'deepseek',
      permission_profile: 'auto',
      allow_lead_routing: true,
      goal: '这是临时项目的真实自主开发验收。以下调度约束只给主控，其他代理只完成自己的 assignment。阶段一：主控亲自按原始字节把 source.txt 复制为 delivery.txt，并用工具验证。分别给 Codex、Claude、Antigravity 各派一项只读独立校验任务：每名协作者用原生文件工具读取以自己 harness 英文小写名命名的 .txt，只返回文件中的完整随机字符串，不加标签或标点。主控不要自己读取这三份探针。首次派工对每名协作者使用 SA_SEND permission_profile=readonly、write=false。收齐三份真实报告后提交 SA_LEARN 记下经验和 SA_FINISH 阶段里程碑。首个里程碑提交后本地项目会收到 PROJECT_INBOX.md；进入 discover 后实际用工具检查，发现具体需求则提交有依据的 SA_GOAL 和 SA_NEXT，执行并验证它，然后再提交里程碑。结束前重新检查项目与替代方案，确实无待办才 SA_EXHAUSTED。只操作这个临时项目，不作外部发布。'
    });
    e.start();
    const expiry = Date.now() + 7 * 60000;
    let last = '';
    while (Date.now() < expiry) {
      const jobs = e.all('SELECT agent,turn,state FROM jobs WHERE ticket=? ORDER BY id', tid);
      const state = JSON.stringify(jobs);
      if (state !== last) {
        console.log(state);
        last = state;
      }
      if (['done', 'failed', 'partial', 'cancelled', 'interrupted'].includes(jobs[0].state)) break;
      if (e.fault) throw Error(e.fault);
      await sleep(180);
    }
    const rootJob = e.root(tid);
    const events = e.all('SELECT * FROM events WHERE ticket=? ORDER BY seq', tid);
    const out = path.join(root, 'events.jsonl');
    fs.writeFileSync(out, events.map((row) => JSON.stringify(row)).join('\n') + '\n', { flag: 'wx' });
    if (rootJob.state !== 'done') e.cancel({ ticket: tid });
    else {
      stopId = await e.create({
        cwd: project,
        lead: 'deepseek',
        permission_profile: 'auto',
        goal: '临时取消验收，请仅核对 source.txt，不修改项目或对外发信。'
      });
      const end = Date.now() + 30000;
      while (!e.one("SELECT 1 FROM events WHERE ticket=? AND kind='native'", stopId) && Date.now() < end)
        await sleep(20);
      e.cancel({ ticket: stopId });
      while ([...e.active.values()].some((ctx) => ctx.job.ticket === stopId) && Date.now() < end + 10000)
        await sleep(30);
    }
    const receipt = require('./engine-live-verify.cjs').verify(root, tid);
    fs.writeFileSync(path.join(root, 'receipt.json'), JSON.stringify(receipt, null, 2));
    console.log(JSON.stringify(receipt));
    if (!receipt.passed) process.exitCode = 1;
  } finally {
    for (const ctx of e.active.values()) e.cancel({ ticket: ctx.job.ticket });
    await e.close();
  }
}
main().catch((err) => {
  console.error(String(err));
  process.exitCode = 1;
});
