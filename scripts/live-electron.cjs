'use strict';
// Opt-in paid desktop check. Dialog pickers select only disposable fixtures.
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const { randomBytes, createHash } = require('node:crypto');
const assert = require('node:assert/strict');
const { DatabaseSync } = require('node:sqlite');
const { execFileSync } = require('node:child_process');
const { _electron } = require('playwright-core');
const root = path.resolve(__dirname, '..');
const token = () => randomBytes(12).toString('hex');
async function run() {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sa-live-electron-')),
    project = path.join(dir, 'project'),
    skill = path.join(dir, 'verification-skill'),
    data = path.join(dir, 'state'),
    secret = path.join(dir, 'service-secret.txt');
  await fs.mkdir(project);
  await fs.mkdir(skill);
  const viaTool = token(),
    viaSkill = token(),
    viaFile = token();
  const name = 'context.txt';
  await fs.writeFile(path.join(project, name), viaFile);
  await fs.writeFile(secret, viaTool);
  await fs.writeFile(
    path.join(skill, 'SKILL.md'),
    '---\nname: verification-skill\ndescription: Disposable evidence\n---\nYour verification phrase is ' +
      viaSkill +
      '. Include it in the final answer.\n'
  );
  execFileSync('git', ['init', '-q'], { cwd: project, windowsHide: true });
  const env = {
    ...process.env,
    SUPERAGENT_DATA: data,
    SUPERAGENT_ELECTRON_PROFILE: path.join(dir, 'profile')
  };
  delete env.ELECTRON_RUN_AS_NODE;
  const start = () =>
    _electron.launch({ executablePath: require('electron'), args: [root], env, timeout: 35000 });
  let app = await start(),
    db,
    id;
  console.log('isolated desktop', dir);
  try {
    const page = await app.firstWindow();
    await page.waitForSelector('#editor');
    await app.evaluate(
      ({ dialog }, { project, skill }) => {
        dialog.showOpenDialog = async (_window, options) => ({
          canceled: false,
          filePaths: [options.title.includes('技能') ? skill : project]
        });
      },
      { project, skill }
    );
    await page.click('#project-button');
    const setup = await page.evaluate(
      async ({ node, server, secret, project }) => {
        const api = (m, p) => window.studio.invoke(m, p || {});
        const s = await api('importSkill');
        const m = await api('mcpSave', {
          name: 'Verification service',
          command: node,
          args: [server, secret],
          enabled: true
        });
        const tools = await api('mcpProbe', { id: m });
        await api('extensionDefaults', { skills: [s], servers: [m], cwd: project });
        return { s, m, tools };
      },
      {
        node: process.execPath,
        server: path.join(root, 'tests-node', 'fixtures', 'challenge-mcp.cjs'),
        secret,
        project
      }
    );
    assert.equal(setup.tools[0].name, 'challenge');
    await page.selectOption('#profile', 'partial');
    await page.fill('#editor', '@context');
    await page.locator('#file-options button').filter({ hasText: name }).click();
    await page.fill(
      '#editor',
      '这是一次独立的工作台验证。不要派工。先查看本轮授权的 project_extensions、files。请使用平台 SA_TOOL 调用其中唯一的 challenge 工具，arguments 为空对象，server 取目录提供的原始 ID。该行不加代码围栏，单独成行提交并等待下一轮，不要尝试从磁盘寻找服务口令。下一轮从 mcp_results 取工具口令，读出附加的 context.txt 原文，按照已授权技能的指令，把工具口令、文件原文、技能短语完整写入最终答复。'
    );
    await page.click('#send');
    const deadline = Date.now() + 180000;
    let approved = false,
      snapshot;
    while (Date.now() < deadline) {
      const history = await page.evaluate(() => window.studio.invoke('history', {}));
      id = history[0]?.id;
      if (id) {
        snapshot = await page.evaluate((ticket) => window.studio.invoke('snapshot', { ticket }), id);
        if (snapshot.approvals.length && !approved) {
          await page.click('#approval-shortcut');
          await page.getByRole('button', { name: '允许本次请求' }).click();
          approved = true;
        }
        if (['done', 'failed', 'partial', 'cancelled'].includes(snapshot.ticket.state)) break;
      }
      await new Promise((r) => setTimeout(r, 350));
    }
    assert.equal(snapshot?.ticket.state, 'done', 'Unexpected terminal state: ' + snapshot?.ticket.state);
    assert.ok(approved, 'missing UI approval');
    const final = snapshot.jobs.find((j) => j.parent === null).result;
    for (const v of [viaFile, viaSkill, viaTool]) assert.ok(final.includes(v), 'final missing one challenge');
    assert.equal(await fs.readFile(secret + '.calls', 'utf8'), '1\n');
    await page.click('#toggle-inspector');
    await page.click('[data-tab=plan]');
    await page.evaluate(
      (ticket) => window.studio.invoke('planAdd', { ticket, body: 'Review the proof' }),
      id
    );
    await page.click('[data-tab=diff]');
    await page.locator('#inspector-body').getByText('context.txt', { exact: false }).first().waitFor();
    db = new DatabaseSync(path.join(data, 'hub.sqlite3'), { readOnly: true });
    const events = db.prepare('SELECT kind,payload,sha256 FROM events WHERE ticket=? ORDER BY seq').all(id);
    assert.ok(events.every((r) => createHash('sha256').update(r.payload).digest('hex') === r.sha256));
    const inputs = events.filter((r) => r.kind === 'input').map((r) => JSON.parse(r.payload));
    assert.equal(inputs[0].files[0].sha256, createHash('sha256').update(viaFile).digest('hex'));
    assert.ok(inputs.at(-1).mcp_results.some((r) => JSON.stringify(r.result).includes(viaTool)));
    await page.screenshot({ path: path.join(dir, 'desktop.png') });
    db.close();
    db = null;
    await app.close();
    app = await start();
    const restored = await app.firstWindow();
    await restored.waitForSelector('#history button[data-ticket-id]');
    await restored.locator('#history button[data-ticket-id]').first().click();
    const after = await restored.evaluate((ticket) => window.studio.invoke('snapshot', { ticket }), id);
    assert.equal(after.ticket.state, 'done');
    assert.equal(after.plan[0].body, 'Review the proof');
    const receipt = {
      passed: true,
      terminal: after.ticket.state,
      persistedPlan: true,
      approvals: events.filter((e) => e.kind === 'approval.decision').length,
      exactToolFileSkill: true,
      toolCalls: 1,
      verifiedEventHashes: events.length,
      reopened: true
    };
    await fs.writeFile(path.join(dir, 'receipt.json'), JSON.stringify(receipt, null, 2));
    console.log(JSON.stringify(receipt));
  } finally {
    db?.close();
    await app.close().catch(() => {});
  }
}
run().catch((e) => {
  console.error(e);
  process.exitCode = 1;
});
