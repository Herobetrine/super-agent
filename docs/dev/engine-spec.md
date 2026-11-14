# Super Agent scheduler: behavior spec for the Rust port

Source snapshot: `E:\Super Agent` working tree, 2026-11-03 (branch `main`, HEAD `ea08b8a`, with uncommitted Rust changes; no JS runtime file was modified).
The citation format is `file:line`. These abbreviations are used:

| Abbrev | File |
|---|---|
| E | runtime/engine.cjs |
| D | runtime/engine-dispatch.cjs |
| PR | runtime/engine-protocol.cjs |
| S | runtime/engine-store.cjs |
| SC | runtime/engine-schema.cjs |
| G | runtime/engine-guides.cjs |
| B | runtime/broker.cjs |
| ST | runtime/studio.cjs |
| X | runtime/extensions.cjs |
| AI | runtime/adapters/index.cjs |
| AC | runtime/adapters/common.cjs |
| CX | runtime/adapters/codex.cjs (CXC = codex-config.cjs) |
| CL | runtime/adapters/claude.cjs |
| DS | runtime/adapters/deepseek.cjs |
| AG | runtime/adapters/antigravity.cjs |
| T1 | tests-node/engine.test.cjs |
| T2 | tests-node/engine-autonomy.test.cjs |
| T3 | tests-node/engine-burst.test.cjs |

Constants (PR:4-14):
- `AGENTS = [codex, claude, deepseek, antigravity]`.
- `PROFILES = [readonly, ask, partial, full, auto]`, in increasing order of authority. The legacy-only value `legacy` is also valid.
- `TERMINAL = [done, partial, failed, cancelled]`. The same set appears in SQL as `endSQL` (S:7).
- `isFull(p)` is `p ∈ {full, auto}`.
- `canWrite(agent, p)` is `isFull(p) || agent ∈ {codex, deepseek}`.
- `validText(x, max=∞)` means: a string with non-empty `trim()` and `length ≤ max`. **Length is in UTF-16 code units, so the Rust port must count UTF-16, not bytes or chars.**
- `ident(x)` is `validText(x, 256)` with no `\p{C}`, `\p{Zl}` or `\p{Zp}` characters.
- `digest` is lowercase-hex SHA-256 of the UTF-8 bytes.
- `childProfiles(p)` is the prefix of PROFILES up to and including `p`. Unknown or `legacy` gives `[readonly]`.
- `now()` is `Date.now()/1000` as an f64 in seconds (S:9).

---

## 1. Persistence: tables, columns, events

### 1.1 DB setup (SC:69-107)
- The file is `<dataDir>/hub.sqlite3`. Pragmas: `busy_timeout=5000; journal_mode=WAL; synchronous=FULL; foreign_keys=ON`. T3 asserts `synchronous=2` and `wal`.
- Every write group runs as `BEGIN IMMEDIATE … COMMIT`, with ROLLBACK on throw (SC:69-79). Transactions are never nested; JS calls `tx()` only from outside other `tx()` calls.
- Migration is the base schema (SC:7-47) plus `ALTER TABLE ADD COLUMN` for each missing column in `additions` (SC:49-67). It runs in one transaction and is non-destructive (T1 "legacy schema migration").
- Prepared statements are cached per SQL string (S:18-22).

### 1.2 Tables and who writes them

| Table | Columns | Engine writes |
|---|---|---|
| `tickets` | id, goal, cwd, writes, lead, created, claude_route('user'), **agent_routes**('{}'), **permission_profile**('legacy'), **objective**, **auto_cycles**(0), **allow_lead_routing**(0), **follow_config**(0) | **INSERT** in create (S:111-124). It sets `claude_route='user'`, `follow_config=1`, `agent_routes=JSON(picked)` and `writes = profile≠readonly`. **UPDATE** sets `objective` on SA_GOAL (E:446, PR:160, PR:199) and on follow when the ticket is auto (S:195). It also does `auto_cycles+1` on each auto-root admission (E:167) and resets `auto_cycles=0` on follow (S:195). ST.saveRoutes updates `agent_routes` (ST:266). |
| `jobs` | id, ticket, parent, sender, agent, body, writing, depth, turn(1), state, result, session, source_turn, msgkey, role(''), assigned_role(''), **dispatch_turn**, **ready_after**(0 REAL), **route_override**, **permission_override**; UNIQUE(parent, source_turn, msgkey) | Root INSERT (S:127-136): `sender='user'`, `agent=lead`, `body=goal`, `writing = writes && canWrite(lead, profile)`, `depth=0`, `state='queued'`. Child INSERT (D:92-110). State and turn updates are covered in §2. |
| `events` | seq PK, ts REAL, ticket, job, turn, kind, payload TEXT(JSON), sha256 | `log(job, kind, data)` (S:35-49): `payload = JSON.stringify(data ?? null)`, `sha256 = digest(payload)`, `turn = job.turn` from the **in-memory** job object (the admission snapshot). It returns the seq. |
| `job_files` | job PK, manifest JSON | Written on create (S:137) and dispatch (D:111). |
| `mailbox` | seq PK→events, target, source, source_turn, msgkey, consumed_turn; UNIQUE(source, source_turn, msgkey) | `recordNote` (S:263-270). A rejected note gets `target=NULL`. `consumed_turn` is set at admission (E:177-181) and on native acceptance (S:291-296). |
| `progress_receipts` | (job, source_turn, msgkey) PK, seq | Written by SA_PROGRESS (D:167-173). |
| `route_defaults` | singleton=1, routes JSON | Written by ST.saveDefaults (ST:234-238). The engine only reads it in create (S:108). |
| `native_routes` | job PK, route JSON | `INSERT OR REPLACE` on each execution (E:258). ST.saveRoutes deletes it for changed agents (ST:259-261). |
| `approvals` | id, ticket, job, turn, action JSON, decision('pending' / allow / deny / expired) | §6 |
| `continuous_runs` | ticket PK, phase('work'), milestone, learning, boundary_job, boundary_seq, probe, success, stalls, last_signal | INSERT on create when profile=auto (S:126). Updated in §5. |
| `studio_plan` | id, ticket, body, done | Studio only (ST:118-120, 359-375). The engine reads it if the table exists (E:170-174). |
| `studio_ticket_extensions`, `studio_tool_receipts`, `studio_servers` | — | Extensions (X:248-250, 328-330, 372-384) |
| `project_knowledge` (Rust port) | id, cwd, kind, body, ticket, job, agent, created, sha256, archived(0) | §4.4. Not in the JS schema; created by the Rust journal opener. |
| `cancel_receipts` (Rust port) | (job, source_turn, msgkey) PK, target, seq | SA_CANCEL idempotency (§4.2). `sa_engine::db::SCHEMA`, `CREATE … IF NOT EXISTS`; the Node engine ignores it. |
| `ticket_failover` (Rust port) | ticket PK, mode(`'readers'`) | A row enables automatic failover of read-only children (§9.2 F). Written by create (`CreateParams.failover`) and `Engine::set_failover`. |
| `job_failovers` (Rust port) | original PK, replacement | Written when the engine fails a child over (§9.2 F). |

### 1.3 Event kinds (payload shape · when · where)
Event row `job`/`turn` are the job the event is logged against.

| kind | payload | when / where |
|---|---|---|
| `user` | goal string | create, root job, turn 1 (S:139) |
| `files` | manifest array | create when there are attachments (S:140) |
| `user.follow` | body string | follow, logged on root (S:183). **Its seq is the "redirect" marker that many checks compare against `job.admission`.** |
| `retry` | `"重试同一逻辑轮次，保留已提交消息的幂等收据。"` | manual retry (S:217) |
| `note` / `note.rejected` | `{job, sender, body, sha256, id?, reason?}`. `sender` is `"<agent> (#<id>)"` or `"user"`. | recordNote (S:254-262). Logged on the source job, or on the receiver for user notes. |
| `note.accepted` | `{seqs:[…], via:"native"}` | adapter acceptNotes (S:299) |
| `note.deferred` | `{seq, reason}` | Codex adapter steering failure (CX:160, 195, 331) |
| `cancelled` | `"用户停止了工单"` | cancel, once per non-terminal job (E:69) |
| `approval.request` | `{approval:id, action}` | E:109 |
| `approval.decision` | `{approval, decision}` | E:85 |
| `approval.unattended_decline` | `{action, reason:"自动工单不会代替用户批准受限子任务。"}` | E:94-97 |
| `turn.admitted` | `{}` | admission. **Its seq becomes `job.admission`** (E:176). |
| `route.requested` | `{provider, model, mode}` | E:259 |
| `input` | the packet object actually sent, possibly compacted | E:363 |
| `running` | `{guide, prompt_sha256}` | E:364 |
| `native` | raw adapter frame (any JSON) | adapters |
| `delta` | string | adapters |
| `message.boundary` | null | adapters |
| `permission.effective` | `{profile, sandbox, approvalPolicy, reviewer}` | CX:273-278 |
| `route.selected` | `{provider, model, mode}` | CX:279 |
| `adapter.permission` | `{profile, scope}` | CL:321, DS:275, AG:182 |
| `adapter.route` | `{provider, requested_model, mode:'cli'}` | DS:276 |
| `protocol.preview_invalid` | `{tag}` | streamed control line with bad JSON (E:284) |
| `auto.protocol` | `{tag, reason}` | invalid control on a continuous primary (E:287) |
| `protocol.progress_invalid` | `{reason: invalid_json \| invalid_fields \| conflicting_id}` | D:139-163 |
| `job.progress` | `{id, summary}` | D:166 |
| `send` | `{to, child, body, sha256, write, files:manifest, role, selection\|null, permission_profile:effective}` | D:112-122 |
| `role` | `{role}` | D:223 |
| `reply` | `{text, sha256, session, state}` | E:484 |
| `failed` / `draining` | reason string | fail (E:539), legacy budget (E:164), draining→failed in wake (E:135, payload = `j.result`) |
| `interrupted` | `"上次执行中断，可能已经产生副作用，继续前请核对实际状态。"` | S:73-74 (non-auto tickets) |
| `auto.recover` | the same reason string | S:71 (auto tickets) |
| `auto.retry` | `{after_seconds, reason}` | E:509, E:529 |
| `auto.backoff` | `{reason:"重复的下一步，退避后重新评估。", after_seconds}` | PR:212 |
| `auto.goal` | the SA_GOAL object as sent | E:447, PR:161 |
| `auto.next` | `{body}` or the SA_NEXT object | E:453, E:460-464, PR:200, PR:214 |
| `auto.finish` | the SA_FINISH object | E:455 |
| `auto.learn` | the SA_LEARN object | PR:143 |
| `auto.milestone` | `{...SA_FINISH, milestone:n+1}` | PR:167 |
| `auto.discovered` | `{objective, evidence, body, probe_seq}` | PR:193-198 |
| `auto.exhausted` | `{...SA_EXHAUSTED, last_probe_seq, last_success_seq}` | PR:179 |
| `mcp.started` | `{id, server, tool}` | X:353 |
| `mcp.result` | `{id, server, tool, state: done\|error\|uncertain, result}` | X:376 |
| `routes.changed` | `{old, new, new_native_session:[agents]}` | ST:271-283 |
| `plan.add` / `plan.done` / `plan.reopen` | `{id}` | ST:365, 373 |
| `knowledge.add` / `knowledge.rejected` / `knowledge.archived` | `{id, kind}` / `{reason}` / `{id, archived}` | Rust port, §4.4 |
| `child.cancelled` | `{id, job, cancelled:[job ids]}` | Rust port: accepted SA_CANCEL, on the issuing job (§4.2) |
| `cancel.rejected` | `{id, job, reason: invalid_json \| invalid_fields \| conflicting_id \| not_child \| terminal, state?}` (`id`/`job` null for the first two) | Rust port: refused SA_CANCEL, on the issuing job (§4.2) |
| `cancelled` (SA_CANCEL / failover) | `"上级任务 #<parent> 取消了此任务"` or `"执行端 <agent> 不可用，已自动改派给 <agent>（任务 #<id>）"` | Rust port: each job stopped by SA_CANCEL or replaced by failover |
| `send` (failover) | the `send` shape with `selection:null`, `write:false`, plus `failover_of: <original job>`, `reason: route_unavailable \| circuit_open` | Rust port: automatic failover, on the parent (§9.2 F) |
| `failover.changed` | `{enabled}` | Rust port: `Engine::set_failover` changed the ticket flag, on the root |

Studio's UI-visible subset is listed in ST:15-40.

---

## 2. Job lifecycle

### 2.1 States
`queued`, `running`, `waiting`, `draining`, `interrupted` plus the terminal states `done`, `partial`, `failed`, `cancelled`.

### 2.2 The loop (E:32-62, 138-206)
- `start()` throws if the engine is closed or stopping. It calls `recover()` and then sets an 80 ms interval running `tick()`.
- `tick()` calls `step()`. Any exception sets `this.fault` and **clears the interval, which stops scheduling for the rest of the process** (E:55-62).
- If an `execute()` promise rejects (for example, a DB error inside `fail`), `fault` is set to `'无法保存任务状态：…'` (E:192-194). `step()` returns early whenever `fault` is set, so the whole scheduler halts.
- `step()` does nothing unless `started && !stopping && !fault`. Otherwise it runs one transaction:
  1. `wake()` (E:126-137):
     - `waiting → queued, turn+1` when either (a) every child has `state ∈ TERMINAL`, which includes having no children, or (b) the job has an unconsumed mailbox row (`target=id AND consumed_turn IS NULL`).
     - `draining → failed` when all children are terminal. This logs `failed` with the existing `result`.
  2. Candidate query (E:143-151):
     - Joins `jobs` with `tickets`, LEFT JOIN `native_routes` (as `bound_route`) and LEFT JOIN `continuous_runs` (as `continuous_ticket`, `floor = boundary_job`).
     - Computes `profile = coalesce(j.permission_override, t.permission_profile)` and `ticket_profile = t.permission_profile`.
     - Filter: `state='queued' AND ready_after<=now() AND writing=0`, ordered by id, `LIMIT 8` (`READS_PER_SLICE`, E:10).
     - If the in-memory `this.writer === null`, it also adds `writing=1 … LIMIT 1`. The combined list is sorted by id.
  3. For each candidate not already in `active`:
     - **Auto root budget** (`profile==='auto' && parent===null`): if there is no continuous_runs row and `auto_cycles >= 64`, set the state to `draining` if any child is non-terminal, otherwise `failed`. Set `result="旧版自动工单达到运行预算，请追加目标继续。"`, log the state kind, and skip the job. Otherwise increment `tickets.auto_cycles`. This also happens for continuous tickets (E:155-168).
     - Build `packet = envelope(job)` (§3). If the `studio_plan` table exists, add `packet.plan = [{id, body, done}]`.
     - Set `state='running'` and log `turn.admitted {}`; that seq becomes `job.admission`.
     - Mark **all** pending mailbox rows for this job as consumed with `consumed_turn = job.turn` (E:177-181).
  4. After commit: put each admitted ctx in `active`. If `job.writing`, set `this.writer = job.id`. Run `execute(ctx)`. When it finishes, remove the job from `active` and clear `writer` if it was this job.
  5. If ≥8 jobs were admitted, schedule another `tick()` on `setImmediate` (E:200-205). Reader concurrency is unbounded; T1 runs 80 readers plus 1 writer at once.

### 2.3 Writer serialization
- **Historical baseline: one declared writer runs at a time, process-wide and across tickets.** This was enforced by the in-memory `writer` slot (E:25, 151, 188, 197). T1 "declared writers serialize across tickets" checks overlapping project directories. The Rust update of 2026-11-09 uses canonical directory overlap instead: unrelated directories and isolated Git worktrees may write concurrently; the same/nested directory remains serialized. Operator validation/integration leases both the source and isolated directory. See [community integration](community-integration.md) and `tests/workspaces.rs` for the current behavior; the numbered JavaScript algorithm above remains a historical reference.
- Readers always run concurrently.
- `writing` means a declared write, not an OS capability. A full or auto child dispatched with `write:false` is a "reader" but still has full OS access (G:39-41).

### 2.4 Transitions table

| From | Event | To | Cite |
|---|---|---|---|
| (new) | create / SA_SEND | queued (turn 1) | S:127-136, D:92-110 |
| queued | admitted | running | E:175 |
| running | reply committed | done / waiting / partial / failed / queued(redirect) | E:409-485 |
| running | adapter error | failed or draining; auto → queued (retry); stopping → interrupted / queued | E:486-540 |
| running | engine stop or restart | auto ticket: queued with turn+1 and dispatch_turn preserved; otherwise interrupted | S:63-84 |
| waiting | children settled or mail | queued, turn+1 | E:128-132 |
| draining | children settled | failed | E:133-136 |
| any non-terminal | cancel | cancelled | E:63-73 |
| failed / interrupted | `retry({job})` | queued, `ready_after=0`, **same turn** | S:204-219 |
| root terminal / … | `follow` | queued, turn+1 | S:175-203 |

### 2.5 Details of each transition

**`live(job)`** (S:50-57) checks that a row exists with the same `id`, `ticket` and `turn` and `state='running'`. Every durable effect of a turn is gated by `live`, so a stale worker becomes inert once the job's turn or state changes.

**`commitReply`** (E:409-485), run in a transaction after a successful adapter run:
1. `children` = rows with `parent=job.id AND id>floor`.
2. `outstanding` = any child is non-terminal, or its state or result differs from the `reports` snapshot sent this turn. In that case the parent must take another turn to read the update.
3. `mail` = an unconsumed mailbox row exists for this job.
4. `follow` = the job is a root and a `user.follow` event exists with `seq > admission`.
5. `state = waiting` if any of outstanding, mail, follow or `ctx.toolFollow` (a new MCP receipt) is true; otherwise `done`. `session = result.sessionId ?? null`.
6. For an auto root without a follow:
   - Parse controls (§4.3). Any `invalid` entry or parse error becomes `correction = '请修正控制消息：' + msg`.
   - Continuous root: `P.advance(...)` (§5) decides the state. If it returns reset, `session = null`.
   - Legacy auto:
     - SA_GOAL → update objective and log `auto.goal`.
     - SA_FINISH with state done: if `status=done` and there are unresolved reports → `waiting` plus `auto.next {body:'请核对失败的子任务并记录证据。'}`. Otherwise log `auto.finish`; `blocked` → `failed`.
     - No FINISH → `waiting` plus `auto.next` (SA_NEXT, or the correction, or `'请核对状态，继续或明确结束工单。'`).
7. A non-auto-root that ends `done` with any child in `partial`, `failed` or `cancelled` becomes **`partial`** (E:468-473).
8. If the job is auto and `follow` is true, `redirect` applies: state becomes `queued` and turn is incremented.
9. `UPDATE jobs SET result=text, session, state, dispatch_turn=NULL, turn=turn+redirect`, then log `reply`.

**`fail(ctx, err)`** (E:486-540) does nothing unless `live`. `reason` is `'任务超时，重试前须核对实际副作用。'` if the turn timed out, otherwise `err.message`. Then:
1. If `stopping` → `interrupt(job)` and return.
2. Define `primary = (profile==='auto' && parent===null)`. **This is not the same as execute's `primary`, which is continuous ∧ root.**
3. If primary, continuous, and there is a `user.follow` after admission: `queued, turn+1, dispatch_turn=NULL, ready_after=0, result=reason`, then log `auto.retry {after_seconds:0, reason:'按新追加的要求重新核对。'}`.
4. If primary and (timed out or `reason` matches `/HTTP (429|50[0234])|connection reset|timed out|rate limit|service unavailable|stream disconnected|暂时不可用|ECONNRESET|ETIMEDOUT/i`):
   - `attempts` = count of `auto.retry` events for this job since the last `reply` or `user.follow` on the job.
   - If continuous **or** `attempts < 2`:
     - `delay = min(300, 2^min(attempts+1, 9))` seconds, giving 2, 4, 8, … 256, 300.
     - Call `interrupt(job)`; this sets queued, turn+1, keeps dispatch_turn, `result` = the generic interrupt text, and logs `auto.recover`.
     - Set `ready_after = now + delay` and log `auto.retry {after_seconds, reason}`.
     - No jitter is applied.
5. Otherwise the state is `draining` if any child is non-terminal, else `failed`. Set `result = reason` and log the state kind.

**`interrupt(job)`** (S:63-76):
- If `ticket_profile==='auto'` (the ticket's profile, so this covers children too): `queued, dispatch_turn=coalesce(dispatch_turn, turn), turn+1, result=reason`, then log `auto.recover`.
- Otherwise: `interrupted, result=reason`, then log `interrupted`.

**Logical turn** is `dispatch_turn || turn`. It is the idempotency key for SA_SEND, SA_NOTE, SA_PROGRESS and SA_TOOL. After an interrupt on an auto ticket, the replayed turn reuses the receipts of the interrupted turn. `commitReply` clears `dispatch_turn`.

**`ready_after`** is an epoch-seconds REAL; a job is admitted only when `ready_after <= now()`. It is set by:
- the fail retry (`now+delay`),
- the advance stall (`Date.now()/1000+delay`, PR:211),
- `follow` (0 for a continuous root, S:191),
- `retry` (0),
- the follow-retry path (0).

### 2.6 Cancellation (E:63-73)
1. In one transaction:
   - `root(ticket)`, which throws `工单不存在` if the ticket is unknown.
   - Expire the ticket's pending approvals.
   - Set **every** non-terminal job of the ticket (queued, running, waiting, draining, interrupted) to `cancelled` and log `cancelled`.
2. Then call `controller.abort()` on every active ctx of that ticket.

Because aborted adapters' later `emit` and commit calls are not `live`, they are dropped. `fail` is a no-op once the job is not live. A cancelled continuous root needs `follow({resume:true})` to restart (S:182, T2).

### 2.7 Recovery after restart (S:77-84; E:35)
`start()` runs `recover()`, which:
- sets every `pending` approval to `expired`;
- calls `interrupt()` on every job with `state='running'`. Auto tickets requeue with turn+1; others become `interrupted`, which needs a manual retry or follow.

T1 asserts recovery runs only on `start()`, not in the constructor.

### 2.8 Shutdown (E:40-54)
1. Set `stopping`, clear the timers and abort every active ctx.
2. Await them. `fail` sees `stopping` and calls `interrupt`.
3. Close the owned native helper, clear the statement cache and close the DB.

### 2.9 Owner lock
- The broker calls the native `claim {directory, pid}` before constructing the Engine (B:75-79).
- The native helper `try_lock`s `<dataDir>/desktop.lock` and fails with `此工单库已有工作台在运行，请先关闭旧窗口`.
- On Windows it also puts the broker pid in a Job Object with `KILL_ON_JOB_CLOSE`, so orphaned CLIs die when the owner exits (native/src/owner.rs:19-34, 61-80).
- Engine.start assumes this lock is held (E:35). Rust must take the same lock file. The required invariant is one scheduler per hub.sqlite3.

### 2.10 Per-turn timeout
`this.timeoutMs = 30*60*1000` (E:26). It is a wall-clock timer per execute (E:271-274) that sets `ctx.timedOut` and aborts. It is total time, not inactivity, and it includes time spent waiting on approvals.

### 2.11 Create path
- ST.create (ST:299-317) validates input. With no cwd it uses `mkdtemp` (`super-agent-project-*`). `selections` default to `route_defaults`, and `allow_lead_routing` defaults to `true`.
- Engine.create (S:85-143):
  - Validates goal, cwd, lead, profile and the routing boolean, then `realpath`s the cwd and checks it is a directory.
  - Runs `extensions.prepare`. If `params.extensions` is set but no extension service is configured, it throws.
  - Runs `snapshot(files)`: at most 16 relative paths, no `..` and no absolute or rooted paths (S:144-174, native `snapshot`). The manifest per file is `{name, bytes, sha256, object_path}`.
  - In the transaction: `routes()` validates each route, which must have exactly the keys `{provider, model, mode∈cli|desktop}` (PR:16-31). Then it inserts the ticket, calls `extensions.bind`, inserts `continuous_runs` if the profile is auto, inserts the root job and logs `user` and `files`.

---

## 3. Per-turn prompt

### 3.1 Prompt string
- `prompt = guide + "\n" + JSON.stringify(packet)` (E:351). Tests parse the packet with `prompt.split('\n').at(-1)`, so **the packet must be single-line JSON on the last line**.
- **guide** is built in this order:
  1. `GUIDE.base` (G:5-47).
  2. Plus `"\n" + GUIDE.continuous` (G:48-70) if this is a continuous primary (continuous ∧ root). Otherwise, if `profile==='auto'` and the ticket is not continuous, plus `"\n" + GUIDE.legacyAuto` (G:71-87). (E:330-336)
  3. Plus the discovery addendum (E:337-340) if continuous primary and `packet.autonomy.phase==='discover'`.
  4. Plus the MCP addendum (E:347-350) if `project_extensions.mcp` is non-empty.
- The guide texts live only in `engine-guides.cjs`. Port them byte-for-byte; `running.guide` is journaled.
- **Size limit**: if the prompt is larger than 6 MiB in UTF-8, call native `compact {packet, guide, directory:dataDir, max_bytes:6Mi}` (E:352-360). See native/src/context.rs:107-.
  - Compact writes the raw packet to `<dataDir>/context-objects/<sha>.json`.
  - It keeps the CONTROL keys, empties the HISTORY keys (`user_updates`, `reports`, `notes`, `roster`, `mcp_results`, `plan`), refills them with excerpts, and adds `full_context {bytes, sha256, object_path, view}`.
  - It then logs `input` (the packet as sent) and `running {guide, prompt_sha256}`.
  - If the result is still larger than 6 MiB it throws `'输入超过 6 MiB 上限，请拆分任务'` (E:366). It also throws for `writing` without permission (`'该任务缺少写权限'`, E:367-368) and for an unknown profile (E:369).

### 3.2 Envelope fields (S:302-375; key order as below)

| field | value |
|---|---|
| ticket, job, turn, depth, agent, sender, role | from the job row (`role` is the job's self-declared role) |
| project | `tickets.cwd` |
| goal | `tickets.goal` |
| assignment | `jobs.body` |
| objective | `tickets.objective \|\| goal` |
| previous_reply | `jobs.result`. This may hold a previous reply, a failure reason or the interrupt text. |
| reports | children with `parent=job AND id>floor`, ordered by id: `{id, sender(agent), role, assignment(body), state, result}`. `floor` is `continuous_runs.boundary_job` or 0. Rust port: plus `failure` (§9.2 F) and, for a child replaced by automatic failover, `replaced_by: <sibling job>`. |
| permission_profile | the effective profile |
| allow_lead_routing | `tickets.allow_lead_routing && parent===null` |
| allowed_child_permissions | `childProfiles(profile)` if routing is allowed and this is the root, otherwise `[]` |
| can_assign_write | `tickets.writes && profile≠readonly` |
| this_turn_can_write | `isFull(profile) \|\| writing` |
| harness_capabilities | `{agent: {read_project:true, write_project: canWrite(agent, profile)}}` for all 4 agents |
| selected_execution | `route_override ?? agent_routes[agent] ?? null` |
| claude_route | `tickets.claude_route` |
| files | this job's manifest, or `[]` |
| notes | **all** mailbox rows targeted at this job, consumed or not, ordered by seq: `{seq, job, sender, body, sha256, id?}` |
| user_updates | every `user.follow` payload (string) for the ticket, in order |
| roster | ticket jobs with `parent IS NULL OR id>floor`: `{job, agent, state, role, parent}` |
| next_step | the latest `auto.next` payload for this job after the most recent `user.follow`, `auto.finish`, `auto.milestone`, `auto.discovered` or `auto.exhausted` in the ticket, or null |
| autonomy | when continuous_runs exists: `{phase, milestone, learning, recent, last_inspection_seq: probe}`. `recent` is the last 8 `auto.milestone`/`auto.discovered` events in ascending order, as `{seq, kind, data}`. Otherwise null. |
| plan | `[{id, body, done}]`, added at admission if the table exists |
| delegate_models | added in executionRoute (§6.4), always present (possibly `{}`) |
| project_extensions | `{skills, mcp}`, only if the extensions service exists. `mcp` is `[]` unless the job is a root and the profile is not legacy or readonly (E:342-346). |
| mcp_results | for roots: `X.results(job)`, i.e. receipts of the latest logical turn plus the last 10, as `{id, state, result}`; for children: `[]` |
| project_knowledge | Rust port, after `recovery` (before `plan`/`delegate_models`), only when non-empty; see §4.4 |

---

## 4. Control lines

### 4.1 Which text is parsed (E:275-326, 389-392)
- **Streamed deltas**:
  - `delta` strings are concatenated with a `pending` remainder and split on `\n`. Complete lines are submitted with `streaming=true`.
  - A `message.boundary` flushes the remainder as one streaming line.
  - This happens only **after** the batch has been durably logged (E:323), and only if the job is still live and not aborted.
- **Final reply**: after the adapter returns, `result.text.split('\n')` and every line is submitted with `streaming=false`.
- `submit` accepts only lines matching `/^SA_(SEND|NOTE|ROLE|TOOL|PROGRESS) /` (the Rust port adds `MEMO`, final pass only, §4.4, and `CANCEL`, streamed and final, §4.2). The tag must be at column 0, followed by one space.
  - `SA_TOOL` is ignored while streaming. External effects run only from the authoritative final text (T2 "MCP effects are final-only").
  - A trailing `\r` is stripped before parsing.
- All submissions go through one serial promise queue per turn.
- `SA_GOAL`, `SA_NEXT`, `SA_FINISH`, `SA_LEARN` and `SA_EXHAUSTED` are parsed **only from the final `result.text`**, inside commitReply (PR:91-125), and only for an auto root.

**Error routing** (Rust update, 2026-11-14; replaces historical E:282-289):
- streaming plus `SyntaxError` (bad JSON) → `protocol.preview_invalid {tag}`; the final pass gets another chance.
- Explicit parameter and authority checks produce a typed `CmdError::Rejected`; final JSON syntax errors use the same rejection path. Log `protocol.rejected {tag,id,logical_turn,command_sha256,reason,executed:false}`, deduplicated by job, logical turn and exact command hash. The rejected command does not execute; later valid controls and the authoritative final reply remain eligible for commit.
- Notify an active parent in the same transaction. A non-primary streaming issuer receives a current-turn correction note. At a successful reply, consume only this physical turn's still-unread correction notes so they cannot cause an extra paid continuation; business notes are untouched. A final-only rejection does not add a self-note.
- A continuous primary additionally retains `invalid.set(tag,msg)` and `auto.protocol {tag,reason}` so its existing autonomous correction contract still applies.
- Database, filesystem and extension/internal failures retain their original error type. Set `fatal`, cancel the native turn immediately, and report the original error rather than the resulting cancellation. Failure to save a rejection receipt is also fatal; it is never silently swallowed.
- Progress views show a fixed label for `protocol.rejected`; command bodies and arbitrary rejection payload fields remain outside the operational projection. The label does not change the job's state to failed.

Lines that do not match the tag regex are ignored. `command()` also ignores lines with no space (D:177-183).

### 4.2 Per-command rules

**SA_PROGRESS** (D:126-175). Validation errors do not fail the job; journal failures remain fatal.
- Skipped silently if aborted, not live, or a ticket-level `user.follow` exists after this job's admission. **That applies to child jobs too** (T1 cancel/follow tests).
- Validation:
  - Bad JSON → `protocol.progress_invalid {reason:'invalid_json'}`.
  - The value must be an object with **exactly 2 keys**. `id` must pass `ident` and be ≤96. `summary` must be validText ≤320 and contain no `\p{Cc}`, `\p{Zl}`, `\p{Zp}` or bidi controls U+202A-202E / U+2066-2069. Otherwise → `invalid_fields`.
- Dedupe key: `(job, logical turn, id)` in `progress_receipts`. The same summary is a silent no-op; a different summary → `conflicting_id` and the original is kept.
- On acceptance: log `job.progress {id, summary}` and insert the receipt. The summary is stored verbatim, including surrounding spaces.

**SA_SEND** (D:5-124):
- `check()` returns a typed rejection (see §4.1) on:
  - a non-object;
  - any of the keys `provider`, `mode`, `selection`, `route`, `route_override`, `permission_override` (`'不可通过派工更换供应商或执行端'`);
  - invalid fields (`'派工参数无效'`): `id` must be validText ≤96; `to` must be in AGENTS; `body` must be validText; `write` must be a boolean (default false); `role` must be a string ≤120 (default ''); `files` must be an array (default []).
- `model` and `permission_profile` are allowed only for a root with `allow_lead_routing`. `permission_profile` must be in `childProfiles(job.profile)`. `model` must pass `ident` and appear in `ctx.catalog[to].models` (the delegate menu).
  - The selection is `{provider: menu.provider, mode: menu.mode, model}`.
  - If the ticket pins `agent_routes[to]`, its provider and mode must match.
- Inheritance: `override = cmd.permission_profile ?? job.permission_override`, so children inherit the parent's narrowing (T2 "readonly child cannot launder"). `effective = override || job.profile`.
- If `write` is set, it needs `ticket.writes`, `effective≠readonly` and `canWrite(to, effective)`.
- Dedupe: look up an existing child by `(parent=job, source_turn=logical, msgkey=id)`.
  - If found, every field must match exactly: agent, body, writing, assigned_role, route_override.model, permission_override and the file name list. Otherwise throw `'同一消息 ID 携带不同的任务、模型、附件或权限'`.
  - An exact match is a silent no-op.
- `dispatch()` steps:
  1. Skip silently if aborted, not live, or superseded. Superseded means a continuous root with a `user.follow` after admission.
  2. Run `check`, then the async native `snapshot(files)` **outside** any transaction.
  3. In a transaction, check `live`, superseded and `check` again, then insert the child (`state=queued`, `depth+1`, `source_turn=logical`, `msgkey=id`, `role=assigned_role=role`, `route_override=JSON(selection)|NULL`, `permission_override=override`). Insert `job_files` and log `send`.
- There are no depth or fan-out limits (G:9-10; T1 nine levels and 80 readers).

**SA_NOTE** (D:224-227, S:220-272):
- `id` must be validText ≤96, otherwise `'无效的信件 ID'`.
- `recordNote(ticket, cmd.job, cmd.body, sourceJob, id)`: `dest` must be a safe positive integer and `body` must be validText.
- Dedupe: `(source, source_turn=logical, msgkey)`. An existing row whose job or body differs returns a typed rejection `'同一 SA_NOTE ID 的内容不一致'`; an identical one returns the old seq. Invalid IDs, destinations or bodies likewise use the typed rejection path for agent controls; user APIs still return the error to their caller.
- Rejections, logged as `note.rejected` with `reason` and a mailbox row with `target=NULL`, without throwing:
  - the receiver is not in the same ticket, or its state is not queued, running or waiting;
  - the budget is exceeded: the count of agent-sourced mailbox rows with a non-null target in the ticket is ≥128. For continuous tickets only the unconsumed rows whose target job is not terminal count.
- A user note (`note()` API, S:273-275) throws on rejection.
- Delivery: rows consumed at admission appear in `notes` (E:177-181). A waiting target is woken (E:131). A running target may receive the note live (§7.4). Otherwise commitReply sees the pending mail and sets the job to `waiting`, which requeues it.

**SA_ROLE** (D:220-223): `role` must be validText ≤120. Update and log `role` only if the value changed, which makes it idempotent.

**SA_TOOL** (D:186-215; final pass only):
- Only the root, with an extensions service, and a profile other than readonly or legacy.
- Fields: `id` validText ≤96, `server` and `tool` pass `ident`, `arguments` is an object.
- The server and tool must be advertised in `packet.project_extensions.mcp`.
- Calls `X.invoke` (X:289-386). Its dedupe key is `(job, logical, key)` with an exact request match. It writes a pending receipt, asks for approval unless the profile is full or auto, logs `mcp.started`, calls the tool (60 s timeout), and stores the result (over 512 KiB → object file). An abort after the call was issued gives the state `uncertain`. It logs `mcp.result` and updates the continuous probe.
- If a new receipt was written, `ctx.toolFollow=true`, which forces `waiting` so the next turn reads `mcp_results` (T2).

**SA_CANCEL** (Rust port, spec §9.2 F; `dispatch::cancel`) `{"id":"key","job":<child>}`:
- Streamed and final lines, like SA_SEND. Skipped silently when aborted, not live, or superseded (continuous root with a `user.follow` after admission).
- Refusals never fail the issuing job; they log `cancel.rejected` on it:
  - bad JSON → `invalid_json`; not an object, `id` not validText ≤96, or `job` not a positive safe integer → `invalid_fields`. Both only on the final pass, so a streamed preview is not reported twice.
  - Dedupe key `(job, logical turn, id)` in `cancel_receipts`. The same id with the same target is a silent no-op; a different target → `conflicting_id` (final pass only), the original receipt is kept.
  - `job` is not a direct child of the issuing job in the same ticket → `not_child`; the child is already terminal → `terminal` (with `state`). Both write a receipt, so replays are silent.
- Acceptance, in one transaction: `cancel_subtree(child)` treats the child and every non-terminal descendant like the operator's cancel treats a ticket (§2.6): expire their pending approvals, set `cancelled`, log `cancelled "上级任务 #<issuer> 取消了此任务"` on each. Then log `child.cancelled {id, job, cancelled}` on the issuer and insert the receipt. After commit, the running turns of the cancelled jobs are aborted (their late output is not live and is dropped).
- The issuer's commit sees the changed child state and goes `waiting`, so its next turn reads the `cancelled` report. A non-auto root that ends `done` with that cancelled child becomes `partial` as usual.
- The guide addendum `guides::REROUTE` (shown when any report has a `failure` or `harness_health` is non-empty) explains SA_CANCEL.

### 4.3 Autonomy controls (`controls(text, continuous)`, PR:91-125)
- Each line is split at the first space. The tag must be exactly one of the fields below.

| tag | required field | availability |
|---|---|---|
| SA_GOAL | `objective` | always |
| SA_NEXT | `body` | always |
| SA_FINISH | `summary` | always |
| SA_LEARN | `lesson` | continuous only |
| SA_EXHAUSTED | `summary` | continuous only |

- The JSON is parsed; a SyntaxError throws. A duplicate tag, a non-object or the field not passing `validText(≤32000)` throws `'<TAG> 缺少有效且唯一的字段'`.
- FINISH and EXHAUSTED: `status ∈ {done, blocked}`. `resolved`, if present, must be an array of `{job: safe int, evidence: validText}`.
- EXHAUSTED: `checks` and `alternatives` must each be an array of 1-32 strings, each validText ≤4000.
- Conflicts: FINISH with NEXT, or EXHAUSTED with any of FINISH, GOAL or NEXT, throws `'同一轮的状态指令相互冲突'`.
- `unresolved(reports, cmd)` (PR:127-134) returns the ids of reports in failed, partial or cancelled state that are not listed in `cmd.resolved`.

### 4.4 Shared project knowledge (Rust port; `crates/sa-engine/src/knowledge.rs`)
Operator requirement: all agents of one project share memory, lessons and progress, and see them every turn. There is no JS counterpart.

- **Table** `project_knowledge(id, cwd, kind ∈ memory|lesson|progress, body, ticket, job, agent, created, sha256, archived)`, index `(cwd, archived, id)`, unique `(cwd, sha256) WHERE archived=0`. DDL in `sa_store::knowledge::SCHEMA`, run by `Db::open` with `CREATE … IF NOT EXISTS`; the Node engine ignores it. A project is the ticket's canonical `tickets.cwd`; `sha256 = digest(body)`.
- **SA_MEMO** `{"kind":"memory|lesson|progress","text":"…"}` from the final reply of any job (lead or child); streamed lines are ignored like SA_TOOL. Exactly these two keys; `text` is trimmed, validText ≤ 2000 UTF-16 units, no control characters other than tab/newline and no bidi overrides. At most 8 SA_MEMO lines per turn. Like SA_PROGRESS, failures never fail the job: they log `knowledge.rejected {reason: invalid_json | invalid_fields | too_many}` without quoting content. Skipped silently when aborted or not live.
- **Dedupe/replay**: no insert when the project has an active row with the same digest, or the same job already recorded it (also when archived since), so retried and replayed turns are no-ops. Each insert logs `knowledge.add {id, kind}` on the writing job.
- **Automatic progress**: commitReply of a root that ends `done` or `partial` inserts `progress` (agent = lead): `工单 #<ticket>（完成|部分完成）目标：<goal ≤300>`, `最新追加：<last user.follow ≤200>` when present, and `结果：<reply without SA_* lines, whitespace collapsed, ≤1200>`.
- **Envelope** `project_knowledge`: appended after `recovery`, only when the project has active entries. Newest first, `{id, kind, body, agent, ticket}`. Each kind first gets its newest entries within 2000 UTF-16 units; the remaining budget (6000 in total, at most 48 entries) goes to the next newest of any kind. Archived rows are excluded. Compaction keeps it as a current (non-history) key.
- **Guide**: `guides::KNOWLEDGE` follows the base/mode guide on every turn.
- **Operator API**: `Engine::knowledge(cwd)` (all rows, newest first, with `archived`), `Engine::add_knowledge(cwd, kind, body, ticket)` (agent `operator`; with a ticket, it must belong to the project and `knowledge.add` is logged on its root; duplicate → error), `Engine::archive_knowledge(id, archived)` (logs `knowledge.archived` on the ticket root if any; restoring over an active duplicate → error). `cwd` is canonicalized when it resolves. Observer reads: `sa_store::knowledge::list(conn, cwd)` or `read(data_dir, cwd)` (opens read-only; a journal without the table reads as empty).
- Tests: `crates/sa-engine/tests/knowledge.rs`.

---

## 5. Continuous autonomy (`advance`, PR:136-216)

The phases are `work`, `discover` and `exhausted`. `settled` means the reply-derived state is `done` (no outstanding children, mail or tool follow). `advance` is called in the commit transaction.

**SA_LEARN**: if `lesson ≠ run.learning`, update it and log `auto.learn` (PR:141-144).

**`checkpoint(phase, kind, data)`** (PR:145-156) logs the event and sets:
- `phase`;
- `boundary_job = max(job id in ticket)`;
- `boundary_seq` = that event's seq;
- `probe = success = stalls = 0`, `last_signal = ''`;
- `milestone += (kind == 'auto.milestone')`.

The new boundary hides older children from `reports`/`roster` via `floor`, and the returned `reset` nulls the session (E:443).

**Work phase** (PR:158-170):
- `goal` → update objective and log `auto.goal`. No evidence is needed.
- `finish && settled`:
  - `status=done` with unresolved reports → hint `'核对失败的子任务并记录解决证据：'+ids`.
  - Otherwise `checkpoint('discover', 'auto.milestone', {...finish, milestone: n+1})` and return `{waiting, reset}`. **Note: `blocked` also closes the milestone and moves to discover; it does not fail the ticket.**
- `end` (EXHAUSTED during work) → hint `'请先用 SA_FINISH 结束阶段，再实际检查当前项目。'`.

**Discover and exhausted phases** (PR:171-203), checked in order:
1. `!settled` → hint `'等待协作者回报并复核。'`.
2. `!run.probe && (goal || end)` → hint `'需要当前探查阶段的实际工具结果。'`.
3. `end`:
   - `done` without `run.success`, or with unresolved reports → hint.
   - Otherwise log `auto.exhausted {...end, last_probe_seq, last_success_seq}`, set `phase='exhausted', stalls=0`, and return state `done` (status done) or `failed` (status blocked).
4. `goal && next && validText(goal.evidence)`:
   - If an `auto.discovered` with the same objective and evidence exists → hint `'此目标和证据已处理…'`.
   - Otherwise `checkpoint('work', 'auto.discovered', {objective, evidence, body: next.body, probe_seq: run.probe})`, update the objective, log `auto.next(next)` and return `{waiting, reset}`.
5. `goal || finish` → hint `'探查阶段需要有 evidence 的 SA_GOAL 和 SA_NEXT，或完整的 SA_EXHAUSTED。'`.

**Probe and success** (E:310-320; X:377-383):
- Applies to each `native` event logged by **any job in a continuous ticket** for which `toolsProof(e)` is not null (PR:68-89). Recognized forms:
  - generic `tool_result` with `callId`;
  - Claude `user` messages with `tool_result` blocks;
  - Codex `item/completed` for commandExecution, mcpToolCall, webSearch or dynamicToolCall;
  - Antigravity `step_update` tool items in DONE, ERROR or FAILED.
- The update is `probe=seq`, plus `success=seq` if the proof is `ok`, and only when `phase='discover' AND boundary_seq < job.admission`. Proof from a turn admitted before the boundary does not count.
- MCP results do the same (state `done` → success).

**Step and stalls** (PR:204-215):
- `step = hint ? {body: hint} : (next || {body: settled ? '请依据当前阶段和实际结果继续。' : '等待协作者回报并复核。'})`.
- `stalls = (settled && step.body === run.last_signal) ? stalls+1 : 0`. Store `stalls` and `last_signal`.
- If `stalls > 0`: `ready_after = now + min(300, 2^min(stalls, 9))` and log `auto.backoff`.
- Always log `auto.next(step)` and return `waiting`.
- A waiting root with no pending children is requeued by `wake()` on the next tick, with turn+1, subject to `ready_after`.

**`follow` on a continuous ticket** (S:184-192) resets to `phase='work'`, `probe/success/stalls=0`, `last_signal=''`, `boundary_seq=followSeq` and root `ready_after=0`. Note that `boundary_job` is **not** changed.

**Legacy 64-turn rule** (E:155-168):
- Applies to an auto ticket with no continuous_runs row; only pre-continuous databases have these, because create always inserts the row.
- The root is stopped (failed or draining) when `auto_cycles ≥ 64` at admission. `follow` resets `auto_cycles`.
- Continuous tickets ignore the budget (T2 sets `auto_cycles=200`).

**Redirects**: a `user.follow` after admission does the following:
- Suppresses streamed and final SA_SEND (continuous root only), SA_PROGRESS (any job) and SA_TOOL (X:322-325).
- Skips control processing.
- Requeues an auto root with turn+1 (E:474-475).
- In `fail`, requeues with `ready_after=0` (E:495-511).

---

## 6. Approvals, permissions, routes

### 6.1 Approvals (E:74-125)
- `approve(ctx, action)` returns false immediately if aborted or if the effective profile is not ask or partial.
- If `ticket_profile==='auto'` it logs `approval.unattended_decline` and returns false. Auto tickets never block on a human, even when a child's profile is narrowed to ask (T2).
- Otherwise it inserts `approvals(ticket, job, turn, action JSON)` and logs `approval.request`. It then polls every 100 ms while not aborted, not closed and live. The first non-pending decision returns `decision==='allow'`.
- On exit it sets any still-pending approval to `expired`.
- `decide({id, allow})` validates its arguments and joins the approval with its job. It requires `job.state==='running' && job.turn === approval.turn` and `decision='pending'`, otherwise throws `'审批已失效'`. It updates the row and logs `approval.decision`.
- Pending approvals for `(job, turn)` expire when execute finishes (E:402-406), on cancel (E:66) and on recover (S:79).
- Action shapes:
  - common scope (AC:279-290): `{agent, action:'workspace_write', cwd, scope:'current_turn', summary}`;
  - Codex (CX:227-234): `{adapter:'codex', method, title, request_id, params, item?}`;
  - MCP (X:339-344): `{tool, arguments, detail}`.

### 6.2 Profiles to adapter
- execute passes:
  - `profile`: `legacy` becomes `partial` when writing and `readonly` otherwise (E:374);
  - `writable = isFull(profile) || writing` (E:375).
- Common `scope()` (AC:268-294, used by Claude, DeepSeek and Antigravity):
  - full or auto → `danger-full-access`; requires writable.
  - not writable → `read-only`.
  - readonly profile or a non-writer harness → throw.
  - ask → asks for approval of `workspace_write` for the turn.
  - partial → `workspace-write`.
- Codex `policy()` (CX:4-20):
  - full or auto → `danger-full-access` with review `never`, native auto-accept.
  - writable ask or partial → interactive review `on-request`. The sandbox is `workspace-write` for partial and `read-only` for ask.
  - otherwise read-only with review never.
  - In non-native mode it disables apps, plugins, hooks and multi_agent and every configured MCP server (CX:95-108).
  - It verifies that the sandbox, approval policy, model and provider the server echoes back match the request (CX:259-270).

### 6.3 Child narrowing
- The allowed child profiles are `childProfiles(parent effective profile)`.
- A child's effective profile is `coalesce(child.permission_override, ticket.permission_profile)` (E:144).
- An override is inherited by grandchildren (D:32).
- Only a root with `allow_lead_routing` may set it (D:27-28).

### 6.4 Routes (E:219-262)
- `assigned = tickets.agent_routes`. The starting selection is the bound `native_routes` row for this job, if any, else `packet.selected_execution`.
- `pick(name, cat)` is `assigned[name]`. If unset: for Claude on a ticket with `follow_config≠1`, `cat.legacy_defaults[claude_route]`; otherwise `cat.default`.
- If `allow_lead_routing`, for each of the 4 agents: `catalogRequest(name)` (concurrent requests per agent are deduplicated in flight only, E:207-215) and `options(cat, pick)`.
  - A successful menu `{provider, mode, default_model, models:[selected, ...other available]}` goes into `ctx.catalog[name]`.
  - **An error or an unavailable result silently omits that agent.**
- `packet.delegate_models = ctx.catalog` is always set.
- Resolve `selected`. **If `options(cat_of_own_agent, selected)` is null, throw `'所选供应商、模型或执行端当前不可用'`.** The engine never substitutes another upstream (E:252-255).
- Persist `native_routes` and log `route.requested`.
- `options()` (PR:33-66) requires all of:
  - `cat.available` is not false;
  - exactly one mode row matching `selected.mode`, with `available===true` and allowing the provider;
  - exactly one provider row, not unavailable;
  - the model id present and not unavailable.
- AI.run additionally requires `selection.mode==='cli'` (AI:36-43).
- **follow_config**: new tickets get 1 (S:114). Old tickets get 0, and their Claude jobs fall back to `legacy_defaults[claude_route]`.
- ST.saveRoutes (ST:239-290) requires every job of the ticket to be terminal. For each changed agent it deletes `native_routes`, nulls `session` and `route_override` on that agent's jobs, and logs `routes.changed`.

---

## 7. Adapter contract

### 7.1 Engine → `adapters` object
- `catalog(agent)` → Promise<Catalog>.
- `catalogRevision(agent)`: a fingerprint of the files the adapter reads (AI:16-18).
- `run(agent, options)` → Promise<{text, sessionId}>. Validated in AI:19-51:
  - prompt is non-empty;
  - cwd is an absolute existing directory, which is then realpath'd;
  - writable is a boolean;
  - `sessionId` matches `/^[\w][\w:.-]{0,220}$/`;
  - the selection mode is `cli`;
  - the signal is not already aborted.
- Run options passed by the engine (E:370-387):
  - `prompt, cwd, selection{provider,model,mode}, profile, writable, sessionId (jobs.session), signal`;
  - `emit(kind, data)` and `emit.batch([[kind, data], …])`: each batch is committed in one transaction, after which deltas are projected. Events are dropped if aborted or not live;
  - `approve(action)` → Promise<bool>;
  - `pendingNotes()` → `[{seq, sender, body}]`, i.e. this job's unconsumed mailbox rows;
  - `acceptNotes(seqs)` marks those rows consumed for this turn and logs `note.accepted` (S:284-301).
- `result.text` must pass `validText`, otherwise `'适配器未返回有效正文'` (E:389).
- `result.sessionId` is stored in `jobs.session` on commit and passed back on the job's next turn.

### 7.2 Catalog shape
```
{
  providers: [{
    id, label, available?, detail,
    models: [{id, label, available?}],
    defaultModel?
  }],
  modes: [{id: 'cli'|'desktop', label, available, detail}],   // desktop always available:false (AC:310-315)
  default: {provider, model, mode},
  legacy_defaults?: {user?, 'cc-switch'?},                     // claude only
  detail,
  downloads: [{label, url}]
}
```
Discovery runs live on **every** `catalog()` and every `run()`; there is no cache.

| Harness | Discovery source |
|---|---|
| Codex | `codex debug models` with an 8 s timeout, custom provider `/models`, or `model_catalog_json` (CXC:87-173) |
| Claude | CC Switch DB rows, the native settings file, `auth status` plus initialize (8 s), and localhost `/v1/models` (3.5 s) (CL:52-266) |
| DeepSeek | YAML, a node probe, CC Switch (DS:65-210) |
| Antigravity | `agy models` (15 s), CC Switch (AG:110-162) |

### 7.3 Per-harness run

**Codex** (CX:81-333): JSON-RPC over `codex app-server --listen stdio://`.
1. Sequence: `initialize`(id 1), `initialized`, then `thread/start` or `thread/resume {threadId, excludeTurns:true}` (id 2). Verify the echoed values, then emit `permission.effective` and `route.selected`, then `turn/start` (id 3).
2. Frames for this thread are emitted as `native`:
   - `item/agentMessage/delta` additionally emits `delta`;
   - `item/completed` for an agentMessage that had deltas emits `message.boundary`;
   - `lastText` = the last non-`commentary` agentMessage text.
3. On `turn/completed` with a matching id: status `completed` returns `{text: lastText, sessionId: threadId}`; any other status throws `rpcError(turn.error)`.
4. Server requests:
   - approvals are mapped to `o.approve` (native accept in full or auto);
   - elicitation and requestUserInput get narrow auto answers;
   - anything else gets error -32601.
5. A JSON-RPC `error` response throws `rpcError`, which keeps **only** an HTTP code from 401/403/429/500/502/503/504 (CX:74-80).
6. **Setup timeout**: 90 s until a turn id is known (CX:139-140, 287, 299).
7. **Cancel**: `turn/interrupt`, then kill the tree after 2.4 s (CX:130-138).
8. **Finally**: end stdin and hard-kill after 1.8 s (CX:319-332).
9. **EOF before completion**: throws `fault || 'Codex 在完成前断开了会话'`. The exit code is ignored.

**Claude** (CL:267-356): `claude -p` with stream-json in and out, `--verbose --include-partial-messages`, plus `--resume <session>`.
- Frames: `native` for everything. `stream_event` text_delta emits `delta`; `content_block_stop` emits `message.boundary`.
- Throws if `system/init.model` differs from the selected model (non-SDK auth only), or on `system/api_retry` with `error==='authentication_failed'`.
- Requires a `result` event with `subtype success`, not `is_error`, a string `result`, a `session_id`, and the same session if resumed. Otherwise throws `'Claude 未返回匹配会话的成功终态'`.

**DeepSeek** (DS:211-293): `node --expose-internals dsh/lib/bin.js --profile headless --patch <tmp policy.yml> --json [--session-id] -`, with the prompt on stdin.
- `thinking` events are skipped.
- `session` sets the session id. `text` emits `delta` and then `message.boundary`. `final` sets the text.
- The temp dir is removed in `finally`.
- Error: `'DeepSeek 未返回匹配会话的完整终稿'`.

**Antigravity** (AG:163-206): `agy` stream-json with `--conversation <id>`.
- `step_update` agent_response: `text_delta` emits `delta`; `DONE` emits `message.boundary`.
- The `result` event must have `status SUCCESS`, `response` and a `conversation_id`. Otherwise `'Antigravity 未返回匹配会话的成功终态'`.

**Process plumbing** (AC:152-266), shared by all harnesses:
- `start` spawns with `windowsHide`, no shell, and `detached` on POSIX. **stderr is drained and discarded** (AC:183).
- Abort runs `killTree`: `taskkill /PID /T /F` on Windows, or SIGKILL to the process group.
- `jsonLines` strictly decodes UTF-8 NDJSON, with a 128 MiB per-line cap, and throws `'无效的原生 UTF-8 NDJSON'`.
- `stream()` checks the exit code after EOF and throws `'<exe> 执行失败'` if it is non-zero.
- `capture()` (catalog probes) has a 10 s default timeout and a 16 MiB cap.

### 7.4 Steering notes into an active turn
Only Codex does this (CX:141-176, 189-198):
1. Every 400 ms, take the first pending note and send `turn/steer {threadId, expectedTurnId, input:[{type:'text', text:'Same-ticket update from the named sender:\n'+JSON({seq, sender, body})}]}`.
2. A response with `result.turnId===turnId` → `acceptNotes([seq])`.
3. Otherwise, or if the note is over 64 KiB, emit `note.deferred` and stop steering for the turn.
4. Any still-pending notes force `waiting` and a new turn.

Other harnesses never consume notes mid-turn. T1 checks that host acceptance is idempotent.

---

## 8. Test cases to port (one line each)

**T1 engine.test.cjs**
1. (T1:86) A streamed SA_PROGRESS split across two deltas is journaled while the root is still `running`; the final replay does not duplicate it; root, child and second-turn progress each produce one receipt (3 total) with the summary byte-exact.
2. (T1:121) Same id with a different summary → `conflicting_id`, original kept. Bad JSON → `invalid_json`. Empty id, summary over 320, a newline in the summary, or an extra key → `invalid_fields`. The job still finishes `done`.
3. (T1:149, cancel) Cancelling while the child is blocked → the child's later streamed and final SA_PROGRESS produce 0 `job.progress` events.
4. (T1:149, follow) Same as 3 but with `follow`; the ticket then finishes `done` and 0 progress events come from the stale child.
5. (T1:173) A streamed SA_SEND with `model:'review', permission_profile:'readonly'` on a full ticket starts the child during the parent's turn. The child gets `selection={test-local, review, cli}`, `writable=false`, `profile=readonly` and the byte-exact body. The final replay adds no second `send`. Every event's sha256 equals the hash of its payload.
6. (T1:203) Two user notes sent during a child turn: `pendingNotes` returns 2; `acceptNotes` twice on the first seq logs one `note.accepted`. The unaccepted note forces a second child turn whose `notes` contain both. The child ends at turn 2. A note to a job in another ticket throws.
7. (T1:240) A child SA_NOTE to the root, both streamed and final, records exactly one `note` whose sha256 is the hash of the exact body `'n\r\n '`.
8. (T1:258) Cancelling an `ask` ticket during a pending approval: `approve` resolves false, a later `decide` throws, the root is `cancelled`, and the late streamed SA_SEND creates no child.
9. (T1:275) Partial ticket: `decide(allow)` delivers true once; a second `decide` throws; the adapter body runs once.
10. (T1:290) Two partial tickets with writer leads: at most 1 writer runs at a time (peak 1), and the second is not admitted while the first runs.
11. (T1:317, ×2 profiles partial and auto) The root dispatches 80 reader and 2 writer children; 80 readers plus 1 writer run concurrently (active=81; states running 81, waiting 1, queued 1). Cancel aborts all 81, all 83 jobs become `cancelled`, and only one writer ever started.
12. (T1:360) A chain of 9 same-harness delegations returns results level by level; the root result is `'verified 0'` and 10 jobs are `done`.
13. (T1:372) An 800 000-character goal is compacted by the real native helper to ≤6 MiB; `full_context.object_path` holds a file whose sha256 matches and whose JSON `goal` is the original.
14. (T1:392) A root attachment is delivered byte-exact (`00 FF 0D 0A`) and `plan` is exposed. Cancelling during the child snapshot creates no child. A `../secret` attachment is rejected at create.
15. (T1:440) An old `tickets` table migrates with `permission_profile='legacy'` and `allow_lead_routing=0`. A `running` job stays running until `start()`, then becomes `interrupted`, and its pending approval becomes `expired`.

**T2 engine-autonomy.test.cjs**
16. (T2:71) Continuous flow: work SA_LEARN+FINISH leads to a milestone. In discover, SA_GOAL+NEXT without a tool proof is rejected; after a proof it is accepted. A restart mid-run preserves `learning`. In the second milestone, EXHAUSTED without a proof is rejected, then accepted after one. Totals: 2 milestones, 1 exhausted, 1 discovered, 1 `job.progress`.
17. (T2:114) Continuous ignores `auto_cycles=200`. A cancelled root stays `cancelled` after restart; `follow` without `resume` throws, and with `resume:true` the root becomes `queued`.
18. (T2:138) Engine close during a streamed SA_SEND and SA_PROGRESS, then restart: the child is not re-dispatched (1 child call, 1 `send`), progress is not duplicated, and a note queued before the restart is delivered byte-exact.
19. (T2:178, updated Rust contract) On `ask` tickets, a SA_SEND with a disabled model, a foreign model, `permission_profile:'full'` or a `provider` key is rejected before any child executes. The ordinary root's reply is preserved with a rejection receipt; internal engine errors still fail the root.
20. (T2:195) On an auto ticket, a child with narrowed `ask` profile calling `approve` gets false immediately and one `approval.unattended_decline` is logged. A user note wakes the waiting parent while the child is still running.
21. (T2:230) A streamed SA_TOOL does not invoke. The final SA_TOOL invokes once, with `profile=full` and an integer `admission`. A new receipt forces a turn 2 whose `mcp_results` show it. `project_extensions.skills` is present.
22. (T2:269) A readonly-narrowed child's grandchild runs with `profile=readonly`, `writable=false` and `allow_lead_routing=false`.
23. (T2:288) A follow during catalog discovery (before execution) suppresses the old turn's SA_SEND and FINISH: there are 0 children, and the only `auto.milestone` is on turn 2 with `user_updates=['new objective']`.

**T3 engine-burst.test.cjs**
24. (T3:12) Three runs of 1000 `native`+`delta` pairs, emitted individually, per frame-pair batch and in batches of 32: each run journals exactly 2000 rows, in order and byte-exact (including `\r\n` and emoji), with valid sha256. The DB uses WAL with synchronous=FULL.

---

## 9. Resilience: current behavior, gaps and Rust design

### 9.1 What happens today

| Failure | Detection today | Classification result | Auto-retry? |
|---|---|---|---|
| **(a) CLI exits mid-turn** | Codex: stdout EOF → `'Codex 在完成前断开了会话'` (CX:317-318); exit code ignored. Claude, DeepSeek, Antigravity: non-zero exit → `'<exe>.exe 执行失败'` (AC:261); exit 0 without a terminal record → `'…未返回匹配会话的成功终态/完整终稿'` (CL:346-354, DS:290-291, AG:198-204). stderr is discarded (AC:183), so the cause is lost. | None of these strings match the retry regex (E:515). | **No, for any job.** The job becomes `failed`, or `draining` if children are live. Continuous and auto roots also stop. |
| **(b) Stream stalls, process alive** | No inactivity watchdog. Codex has a 90 s timeout only until the turn id is known (CX:139). After that, only the engine's 30-min wall clock applies (E:26, 271-274). | Codex setup timeout `'Codex 建立轮次超时'` does not match. The 30-min engine timeout sets `ctx.timedOut`. | Only the auto root (`profile=auto ∧ parent=null`) after the 30-min timeout: continuous without limit, legacy at most 2. Every other job → `failed`. |
| **(c) Provider or model unavailable** | Before spawn, the live catalog probe (E:254-255) → `'所选供应商、模型或执行端当前不可用'`. Inside the adapter, `select()` → `'已选供应商或模型失效，请刷新配置'` / `'所选执行端不可自动执行'` (AC:295-309). Catalog probes fail on short timeouts (Codex `debug models` 8 s, Claude `/v1/models` 3.5 s, agy 15 s), so a transient probe failure looks like "unavailable". | Does not match the regex (`暂时不可用` ≠ `当前不可用`). | **No.** Permanent `failed`. The route stays bound (`native_routes`), and saveRoutes needs every job terminal. |
| **(d) Transient network / 429 / 5xx** | Codex: JSON-RPC error or failed `turn/completed` → `rpcError` keeps only the HTTP code (CX:74-80). Claude: the CLI retries internally and emits `system/api_retry` (logged as `native` only); the final `is_error` result is collapsed into the generic message. DeepSeek and Antigravity: generic message. | Only Codex `HTTP 429/500/502/503/504` matches the regex. `ECONNRESET` and the like never reach the regex because adapters rewrite messages. | Auto root only: delay `min(300, 2^(n+1))` s, no jitter. Continuous: unlimited. Legacy auto: 2. Children and non-auto roots: never. |

**Session behavior on retry**
- `interrupt()` keeps `jobs.session`, which is the session from the **last successful reply**. The retried turn therefore resumes the previous native session.
- The failed turn's own new thread or session id is never persisted (only commitReply writes `session`, E:476-483). So a job whose first turn fails restarts with a fresh native session.
- Whether a resumed native session contains the half-finished turn depends on the CLI. Claude docs: an interrupted turn is left as-is on `--resume` unless `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1`.
- The route stays bound via `native_routes`.
- Streamed SA_SEND, SA_NOTE and SA_PROGRESS are idempotent across the retry, because `dispatch_turn` is kept (S:67).

**What the lead sees**
- A failed child appears in the parent's next `reports[]` as `{state:'failed', result:'<adapter message>'}` and in `roster`.
- The parent is woken only when **all** children are terminal (E:127-131). A child sitting in retry backoff keeps the parent waiting, with no notice.
- The parent's own retry shows `previous_reply = '上次执行中断，可能已经产生副作用…'`. The actual error is only in the `auto.retry` event, which is not in the envelope.
- An unavailable harness is silently dropped from `delegate_models` (E:240-242).
- The lead *can* re-route manually, by sending a new SA_SEND with a new id to a different `to`, but it gets no structured cue that the failure was transient or route-level.
- A non-auto root that finishes with a failed child becomes `partial`.

**UI**: Studio shows `failed`, `interrupted`, `auto.retry`, `auto.recover` and `auto.backoff` events, and each job's `ready_after` (ST:15-42). There is no "stalled", "retrying" or "harness degraded" state.

**Gaps**
1. There is no inactivity watchdog. The 30-min wall clock both kills legitimate long turns and lets dead streams hang for 30 min.
2. Retry is regex-on-message, limited to auto roots, and blind to structured errors (Codex `turn.error`, Claude `api_retry.error` and `error_status`, exit codes, stderr).
3. Process crashes and "unavailable" (including transient catalog probe failures) are permanent.
4. Children and non-auto tickets never retry.
5. There is no jitter, no Retry-After handling and no per-route circuit breaker. All jobs on a dead route retry in lockstep.
6. The failed turn's native session id is lost, so a retry cannot resume the turn that actually ran.
7. The lead gets no structured failure info or harness health, and is not woken while a child is stuck retrying.
8. There is no failover to another harness, automatic or assisted. The root cannot be re-routed while the ticket is live.
9. `fault` halts the entire scheduler on one DB error (E:59-60, 192-194).

### 9.2 Rust design (proposal; not implemented, not verified)

**A. Configuration** (`ResilienceConfig`, stored per harness, overridable per ticket; values in ms). These are proposed defaults, not measured.

| key | default | meaning |
|---|---|---|
| `spawn_first_frame_ms` | 60 000 | from process spawn to the first stdout frame |
| `setup_ms` (Codex) | 90 000 | until `turnId` is known (keeps today's CX:139) |
| `model_idle_ms` | 360 000 | no frame while no tool is open. Codex's own `stream_idle_timeout_ms` defaults to 300 000, with `stream_max_retries`=5 and `request_max_retries`=4 (Codex config reference), so the watchdog should outlast one native retry window. |
| `tool_idle_ms` | 1 800 000 | no frame while a tool item is open |
| `turn_hard_cap_ms` | 14 400 000 | wall-clock safety cap; replaces the 30 min |
| `interrupt_grace_ms` | 2 400 | native interrupt → kill tree (as CX:133) |
| `retry_base_ms` / `retry_cap_ms` | 2 000 / 300 000 | backoff |
| `retry_max` | transient 5, crash 3, stall 3, unavailable 3; continuous root unlimited but at most 30 per hour | per job, per logical turn |
| `breaker_threshold` / `breaker_window_ms` / `breaker_open_ms` | 3 / 600 000 / 120 000 | per `(agent, provider, model)` |
| `notify_parent_after` | 1 | failed attempts before the parent is told |

**B. Per-adapter heartbeat and stall watchdog**
- Each adapter task owns an `Activity` record, `{last_frame_at, open_tools: set, phase: Spawning | Setup | Streaming | ToolRunning | AwaitingApproval | Closing}`, kept in memory only.
- Every stdout frame updates `last_frame_at`. This includes Codex `error`/retry notifications, Claude `system/api_retry` and `stream_event`, and steering responses.
- Open-tool tracking:
  - Codex: `item/started` → `item/completed` for commandExecution, mcpToolCall and fileChange.
  - Claude: `tool_use` id → `tool_result`.
  - Antigravity: `step_update` tool non-terminal state.
  - DeepSeek: tool events, if any. **The DSH event schema for tools is not verified; until it is, use `model_idle_ms` only.**
- `AwaitingApproval` and `Setup` with a native retry announced pause or extend the timers.
- A 1 s tokio interval compares elapsed time with the threshold for the current phase. On breach:
  1. Log `turn.stalled {phase, idle_ms, open_tools}` once.
  2. Send the native interrupt (Codex `turn/interrupt`; Claude end stdin, then kill; others kill), then `kill_tree` after `interrupt_grace_ms`.
  3. Classify the failure as `Stalled`.
- Before acting on a stall, check the process is still alive (`try_wait`). If it has exited, classify as a crash instead.
- Capture a bounded stderr tail (8 KiB ring) **in memory only**. It is used only by the classifier and is never journaled raw, because it may contain credentials (AC:183 intent). Only the class and a redacted short code are logged.

**C. Error classification.** The adapter returns `AdapterError {class, retryable, http_status?, retry_after_ms?, code, message_zh}`. Sources, in priority order:
1. Structured native signals:
   - Codex `turn.error.codexErrorInfo` and JSON-RPC error data. **The exact enum names must be checked against the installed `codex app-server generate-json-schema`.**
   - Claude `api_retry.error` (`rate_limit`, `overloaded`, `server_error` → transient; `authentication_failed`, `billing_error`, `oauth_org_not_allowed`, `account_on_hold`, `model_not_found`, `invalid_request` → permanent or route-level), `error_status` and `no_response`.
   - The Claude result `subtype` and `is_error`.
2. Exit status and signal.
3. The engine's own timers.
4. Last, a regex over the stderr tail and the message: the current E:515 list plus `ECONNREFUSED`, `EAI_AGAIN`, `socket hang up` and `overloaded`.

| class | examples | retryable | action |
|---|---|---|---|
| `Transient.Network` | reset, DNS, TLS, no_response | yes | backoff + resume |
| `Transient.RateLimited` | 429, `rate_limit` | yes | honor Retry-After, else backoff |
| `Transient.Overloaded` | 5xx, `overloaded` | yes | backoff, counts toward breaker |
| `Stalled` | watchdog | yes (≤3) | resume |
| `ProcessCrashed` | non-zero exit or EOF with no terminal record | yes (≤3) | resume |
| `RouteUnavailable` | `options()` null, `select()` failure | conditional | Re-probe the catalog once after 5 s, bypassing in-flight dedupe. If the provider row's reason is a probe failure (timeout or IO), retry with backoff (60 s, 120 s, 300 s). If the model or provider was removed from config, the error is permanent. Catalog needs a structured `unavailable_reason: probe_failed \| not_configured \| auth`. |
| `Permanent.Auth`, `Permanent.Billing`, `Permanent.Permission`, `Permanent.Protocol`, `Permanent.InputTooLarge`, `SessionMismatch` | — | no | fail now. `SessionMismatch` may retry once with `session=null`. |
| `Cancelled`, `Superseded` | abort, follow | n/a | today's paths |

**D. Bounded retry with jitter and session resume.** This applies to **all** jobs, not only auto roots.
1. **Persist the native session early.** A new table `native_sessions(job, turn, agent, route, session, state)` gets a row as soon as the session id is known:
   - Codex `thread/start` result `thread.id` (CX:271);
   - Claude `system/init.session_id`;
   - DeepSeek `session` event (DS:280);
   - Antigravity: only in `result` today, so resume after a mid-turn failure is not possible for it. Check whether agy emits the id earlier.
2. **On a retryable failure**, in one transaction:
   - Apply today's `interrupt` semantics to every ticket: `queued`, `dispatch_turn = coalesce(dispatch_turn, turn)`, `turn+1`. Turn+1 matters because it makes the dead worker non-live.
   - Set `retry_attempt+1`, `last_error_class`, `ready_after = now + delay` with `delay = min(cap, base·2^n)·U(0.5, 1.0)`, or `max(retry_after, that)`.
   - Set `jobs.session` = the newest session from `native_sessions` for this job and route. Interrupt that turn natively first where possible: Codex `thread/resume` then a new `turn/start`; Claude `--resume`.
   - Log `turn.retry_scheduled {class, attempt, max, after_ms, resume_session: bool}`. Keep emitting `auto.retry {after_seconds, reason}` for UI compatibility.
3. **On exhaustion**: `failed` or `draining` with `result` = the redacted class message, plus `turn.gave_up {class, attempts}`.
4. **Resume prompt.** Send the full envelope (idempotency is unchanged) plus a new field `recovery: {class, attempt, previous_turn, session_resumed, side_effects_possible: true}`, and a one-line guide addendum: "the previous attempt was interrupted by …; inspect actual state before repeating effects". Keep `previous_reply` as the last real reply instead of overwriting it with the interrupt text: store the interrupt text in `recovery` and stop clobbering `result`.
5. **Writer slot.** Release it on failure; the retry re-enters through normal admission.
6. **Uncertain side effects.** MCP `uncertain` receipts already surface in `mcp_results`.

**E. Circuit breaker and health.**
- An in-memory `HarnessHealth` per `(agent, provider, model)`: `Healthy → Degraded` after 1 failure in the window, `Open` after 3, then `HalfOpen` after `breaker_open_ms` (a single probe admission).
- While `Open`, admission skips jobs bound to that route. Set their `ready_after = open_until` instead of burning attempts.
- Log transitions only: `harness.health {agent, provider, model, state, reason_class, until}`.
- After restart, rebuild health from recent `turn.retry_scheduled` and `turn.gave_up` events.

**F. Surfacing to the lead (re-route, not silent failover).** This keeps the invariant "never substitute a different upstream" (E:252-253).
- Add a `failure` field to `reports[]`: `{class, retryable, attempts, max, next_retry_at, harness, route}`.
- Add envelope `harness_health: {agent: {state, reason_class, retry_after_s}}`.
- `delegate_models` keeps unavailable agents as `{available:false, reason}` instead of dropping them.
- **Early wake.** After `notify_parent_after` failed attempts of a child, insert a platform mailbox note to the parent. Use `source=NULL` so it does not count toward the 128 budget, with sender `platform`, body `{child, class, attempt, retrying_until}`. Under the existing wake rule (E:131) the lead gets a turn while the child is still in backoff.
- **New optional control** `SA_CANCEL {"id","job"}`, restricted to the job's own direct children and idempotent on `(job, logical, id)`. With it the lead can stop a retrying child and SA_SEND the same body to another harness with a new id.
- **Guide addendum.** "If a report has failure.retryable=false or harness_health shows open, re-dispatch to a different harness from delegate_models; do not resend to the failing one."
- **Optional automatic failover**, off by default, ticket flag `failover:'readers'`. Only for `write:false` children with `RouteUnavailable` or after the breaker opens: the engine creates a sibling job with the same body on the first healthy harness in a configured order, logs `send` with `failover_of`, and marks the original `cancelled`. Writers and the root never auto-fail-over.

**Implemented in the Rust port (2026-11-05)**:
- `SA_CANCEL`: see §4.2. The REROUTE addendum is shown when any report carries a `failure` (retrying or given up) or `harness_health` is non-empty, and documents SA_CANCEL.
- Failover: `CreateParams.failover: Option<bool>` (default off) inserts `ticket_failover(ticket,'readers')`; `Engine::set_failover(ticket, enabled)` / `Engine::failover(ticket)` change and read it on existing tickets. Triggers, only for a child (`parent≠NULL`) with `writing=0` on such a ticket whose parent is `queued`, `running` or `waiting`:
  1. `fail` with an adapter error of class `RouteUnavailable` (including catalog probe failures and removed routes), unless the runner decided to rotate providers on the same harness (`route.rotated` goes first);
  2. `fail` when the runner decided `WaitForRoute` or the job's route is now `open`, or a fresh route selection hit an open breaker (`TurnError::Wait`);
  3. admission of a queued child whose bound route is circuit-open (instead of `turn.deferred`).
- Target: the first agent in `AGENTS` order that has an adapter, is not the failing agent nor any agent earlier in this failover chain (`job_failovers`), and, when the parent's latest `input` has a non-empty `delegate_models`, is listed there as available with a route `{provider, models[0], mode}` that is not `open`/`half_open`. Without a delegate menu any configured harness without an open route qualifies. No target → the normal retry/give-up path.
- Effect, in the failing transaction: insert the sibling (`parent`, `sender`, `depth`, `source_turn`, body, `role=assigned_role`, `permission_override` copied; `writing=0`; `route_override=NULL`, so the ticket route or catalog default of the new agent applies; `msgkey = <original msgkey>#failover:<original id>`), copy `job_files`, log `send {to, child, body, sha256, write:false, files, role, selection:null, permission_profile, failover_of, reason}` on the parent (its current turn), record `job_failovers`, then `cancel_subtree(original)` with `cancelled "执行端 <agent> 不可用，已自动改派给 <agent>（任务 #<id>）"`. Running turns of the cancelled subtree are aborted after commit.
- A replaced child shows `replaced_by` in `reports`; it does not make a non-auto root `partial` and is not `unresolved` for SA_FINISH (its sibling's own state counts).
- **Root.** Allow `saveRoutes` for the root's agent when the root is `failed` or `interrupted` with class `RouteUnavailable` or `Open` and every child is terminal or waiting. Today every job must be terminal (ST:246-253). The UI then offers "switch route and retry".

**G. UI status.** The snapshot API exposes per running job:
- `activity: {phase, idle_ms, open_tools, last_frame_at}` from memory;
- `retry: {attempt, max, class, next_at}` from DB columns;
- `harness_health`.

| Badge | Condition |
|---|---|
| `运行中` | frames within 30 s |
| `等待上游` | idle over 30 s, below threshold, no tool open |
| `工具执行中` | a tool is open |
| `疑似卡住` | idle over 80 % of threshold |
| `已中断，重试中 n/max · mm:ss` | retry scheduled |
| `执行端降级/熔断` | harness health degraded or open |
| `已放弃（分类）` | gave up |

Add the new kinds to the VISIBLE list: `turn.stalled`, `turn.retry_scheduled`, `turn.gave_up`, `harness.health`.

**Implemented in the Rust port (2026-11-05)**: `Engine::activity(&self, ticket: i64) -> Vec<sa_engine::JobActivity>` (module `activity`), ordered by job id.
- Running jobs, from memory: each admitted turn keeps a `Tracker` updated from every adapter event the engine receives (the same heartbeat rules as the watchdog: `ToolStarted/ToolFinished` open tools, `NativeRetry{after}` extends the model limit, `AwaitingApproval` and in-flight `TurnHost::approve` pause). Fields: `job, agent, phase, idle_ms, open_tools (≤16), last_frame_at, started_at (admission, epoch s), threshold_ms, health, retry:None`.
- `phase`: `setup` (route selection and prompt assembly), `spawning` (native turn started, no frame; limit `first_frame`), `streaming` (limit `model_idle` + native retry extension), `tool_running` (limit `tool_idle`), `awaiting_approval` (no limit), `closing` (adapter returned, committing), `retry_wait` (queued). `threshold_ms` comes from `TurnRunner::stall_limits()` (new defaulted trait method; `ResilientRunner` returns its `ResilienceConfig`, `PassThroughRunner` none).
- Queued jobs with `ready_after > now` and a journaled retry since their last `reply`/`user.follow`/`retry`/`turn.gave_up`: `retry: {attempt, max, class, next_at}`. `attempt` counts `turn.retry_scheduled` and `route.rotated`; `max` is the latest `turn.retry_scheduled.max` (null for continuous-root overflow, rotation and route waits); `class` is its class, the rotation reason, or `circuit_open` for `turn.deferred`; legacy `auto.retry`-only jobs report class `transient`. `next_at = ready_after`.
- `health`: the runner's breaker state for the job's route (running: the selected route; queued: `native_routes`), `healthy` when unknown.
- Cost: one scheduler-lock snapshot, one per-turn tracker lock, one breaker snapshot, then one indexed query for queued jobs plus one `events_ticket_kind` lookup per retrying job. Locks are never nested; a journal error yields only the running part.
- Badge `JobActivity::badge() -> (&'static str, BadgeTone)` in precedence order: retry scheduled → `已中断，重试中` (Warning), or `执行端熔断` (Danger) when the route is open; `等待审批` (Waiting); idle over 80 % of `threshold_ms` → `疑似卡住` (Danger); route open → `执行端熔断` (Danger); degraded or half-open → `执行端降级` (Warning); tool open → `工具执行中` (Busy); idle over 30 s → `等待上游` (Waiting); otherwise `运行中` (Active). `JobActivity::badge_text(now_secs)` adds `n/max` (or `n`) and the `mm:ss` countdown to `next_at`. `已放弃（分类）` is not an activity badge: a given-up job is terminal and the UI reads `turn.gave_up`.

**H. Engine robustness.**
- A DB error in a single job's commit should mark that job `interrupted` and log it, with exponential retry of the transaction on `SQLITE_BUSY`. It should not set a global `fault`.
- Keep the global fault only for schema or IO corruption.

**I. Tests to add** (fake adapters):
1. A stalled stream is killed at `model_idle_ms` and retried with a resumed session.
2. An open tool extends to `tool_idle_ms`.
3. A crash with exit≠0 is retried ≤3 times, then gives up with a class.
4. A 429 with Retry-After is honored.
5. Jitter stays within bounds.
6. The breaker opens after 3 failures and blocks other jobs on that route without consuming their attempts.
7. The parent gets a platform note after the first child failure and can SA_CANCEL plus re-dispatch.
8. A catalog probe timeout is classified `RouteUnavailable` (retryable), while a removed model is permanent.
9. The early-persisted session id is reused on retry.
10. Late frames from a killed attempt are dropped (turn+1 makes them non-live).

**Sources for external facts** (fetched 2026-11-03):
- Claude Code headless docs, `system/api_retry` fields and SIGTERM/resume behavior: https://code.claude.com/docs/en/headless
- Codex config reference, `stream_max_retries`=5, `request_max_retries`=4, `stream_idle_timeout_ms`=300000 under `model_providers.<id>`: https://learn.chatgpt.com/docs/config-file/config-reference

Not verified here:
- whether the Codex app-server emits per-retry `error` notifications with `willRetry`;
- the exact `codexErrorInfo` enum names;
- whether the built-in `openai` provider honors these keys without a custom provider block;
- the DSH and agy error and event schemas.
