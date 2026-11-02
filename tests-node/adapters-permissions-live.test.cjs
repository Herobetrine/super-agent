'use strict';
// Set SUPERAGENT_NODE_PERMISSIONS_LIVE=1 to spend real Codex inference calls.
// The callback accepts only one exact fixture PowerShell command in a new disposable folder.
const test = require('node:test'),
  assert = require('node:assert/strict'),
  fs = require('node:fs'),
  os = require('node:os'),
  path = require('node:path'),
  crypto = require('node:crypto');
const { run } = require('../runtime/adapters/index.cjs');
test(
  'Codex native read-only approvals, isolated workspace writes, and full-access operating-system writes',
  {
    skip: process.platform !== 'win32' || process.env.SUPERAGENT_NODE_PERMISSIONS_LIVE !== '1',
    timeout: 350000
  },
  async () => {
    const parent = path.join(os.homedir(), '.local/share/superagent/checks');
    fs.mkdirSync(parent, { recursive: true });
    const base = fs.mkdtempSync(path.join(parent, 'node-codex-permissions-'));
    console.log('native permission directory', base);
    const selected = process.env.SUPERAGENT_TEST_CODEX_SELECTION
      ? JSON.parse(process.env.SUPERAGENT_TEST_CODEX_SELECTION)
      : null;
    async function check(profile, type, outside) {
      const dir = path.join(base, profile + '-' + type),
        cwd = path.join(dir, 'project');
      fs.mkdirSync(cwd, { recursive: true });
      const dest = path.join(outside ? dir : cwd, 'proof.txt'),
        nonce = crypto.randomBytes(8).toString('hex');
      const cmd = "Set-Content -LiteralPath '" + dest + "' -Value '" + nonce + "' -NoNewline -Encoding ascii";
      const escalation = profile === 'ask' || (profile === 'partial' && outside);
      const rec = { profile, type, outside, requests: [], completed: false };
      try {
        await run('codex', {
          cwd,
          writable: true,
          profile,
          selection: selected,
          signal: AbortSignal.timeout(95000),
          prompt:
            'Use only the shell exec_command tool to run this exact PowerShell command once:\n' +
            cmd +
            (escalation
              ? '\nSet sandbox_permissions=require_escalated and provide a brief justification.'
              : '') +
            '\nIf denied stop without retrying. Briefly report the outcome.',
          emit: (k, data) => {
            if (k === 'permission.effective') rec.native = data;
          },
          approve: (a) => {
            const raw = a.params?.command || '',
              i = raw.indexOf(' -Command '),
              left = raw.slice(0, i).replace(/^"|"$/g, ''),
              tail = raw.slice(i + ' -Command '.length);
            const exact =
              i >= 0 &&
              /^(pwsh|powershell)(\.exe)?$/i.test(path.win32.basename(left)) &&
              [cmd, '"' + cmd + '"', '"' + cmd.replace(/\\/g, '\\\\') + '"'].includes(tail);
            const allow =
              a.method === 'item/commandExecution/requestApproval' &&
              escalation &&
              type === 'accept' &&
              exact;
            rec.requests.push({ method: a.method, exact, allow });
            return allow;
          }
        });
        rec.completed = true;
      } catch (e) {
        rec.error = e.message;
      }
      rec.file = fs.existsSync(dest);
      rec.exactBytes = rec.file && fs.readFileSync(dest).equals(Buffer.from(nonce));
      rec.pass =
        type === 'deny'
          ? rec.requests.length > 0 && !rec.file && !rec.requests.some((r) => r.allow)
          : rec.completed &&
            rec.exactBytes &&
            (escalation ? rec.requests.some((r) => r.allow) : rec.requests.length === 0);
      return rec;
    }
    const data = [];
    for (const pair of [
      [
        ['ask', 'accept', false],
        ['ask', 'deny', false]
      ],
      [
        ['partial', 'workspace', false],
        ['partial', 'accept', true]
      ],
      [
        ['full', 'external', true],
        ['auto', 'external', true]
      ]
    ])
      data.push(...(await Promise.all(pair.map((args) => check(...args)))));
    fs.writeFileSync(path.join(base, 'receipt.json'), JSON.stringify(data, null, 2));
    assert.ok(
      data.every((d) => d.pass),
      'check the native permission receipt at ' + base
    );
  }
);
