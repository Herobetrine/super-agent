'use strict';
// Compares the two installed pipelines on the same JSON fixture. No model calls.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const { performance } = require('node:perf_hooks');
const { Native } = require('../runtime/native.cjs');
const median = (arr) => arr.toSorted((a, b) => a - b)[Math.floor(arr.length / 2)];
async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sa-benchmark-'));
  const worker = new Native();
  try {
    const packet = {
      ticket: 1,
      agent: 'deepseek',
      permission_profile: 'readonly',
      goal: 'review the project',
      user_updates: ['latest instruction'],
      reports: Array.from({ length: 14000 }, (_, id) => ({
        id,
        state: 'done',
        sender: 'codex',
        result: 'review 结果 🧪\n'.repeat(50)
      }))
    };
    const guide = 'benchmark guide';
    const fixture = path.join(dir, 'fixture.json');
    fs.writeFileSync(fixture, JSON.stringify(packet));
    const py = [
      'import json,sys,time,pathlib',
      'from continuous_context import compact_packet',
      'root=pathlib.Path(sys.argv[1]); packet=json.loads((root/"fixture.json").read_text(encoding="utf-8")); times=[]',
      'for i in range(6):',
      ' start=time.perf_counter(); p=compact_packet(packet,"benchmark guide",root/"legacy",262144); times.append(1000*(time.perf_counter()-start))',
      ' assert p["reports"][-1]["id"]==13999 and p["user_updates"]==packet["user_updates"]',
      'assert json.loads(pathlib.Path(p["full_context"]["object_path"]).read_text(encoding="utf-8"))==packet',
      'print(json.dumps(times[1:]))'
    ].join('\n');
    const old = spawnSync('python', ['-X', 'utf8', '-c', py, dir], {
      cwd: path.resolve(__dirname, '..'),
      encoding: 'utf8'
    });
    if (old.status !== 0) throw Error(old.stderr || 'Python baseline failed');
    const current = [];
    for (let i = 0; i < 6; i++) {
      const start = performance.now();
      const p = await worker.call('compact', { packet, guide, directory: dir, max_bytes: 262144 });
      current.push(performance.now() - start);
      assert.equal(p.reports.at(-1).id, 13999);
      assert.deepEqual(p.user_updates, packet.user_updates);
      if (!i) assert.deepEqual(JSON.parse(fs.readFileSync(p.full_context.object_path)), packet);
    }
    const a = median(JSON.parse(old.stdout)),
      b = median(current.slice(1));
    console.log(
      JSON.stringify({
        inputBytes: fs.statSync(fixture).size,
        outputBudget: 262144,
        samples: 5,
        pythonMedianMs: +a.toFixed(1),
        nodeRustMedianMs: +b.toFixed(1),
        speedup: +(a / b).toFixed(2)
      })
    );
  } finally {
    await worker.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
main().catch((e) => {
  console.error(e);
  process.exitCode = 1;
});
