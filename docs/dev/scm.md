# GitHub PR feedback

`sa-engine::scm` adds operator-bound, read-only PR observations. It uses the installed GitHub CLI's existing authentication and runs only `gh pr view URL --json ...`. It never writes a GitHub comment, pushes, merges or changes a local job's scheduling/permission state.

## Integration contract

- Execute `SCHEMA` when opening the journal. Tables are additive and compatible with old journals.
- Call `bind(q, job, url)` in a transaction after an operator selects a job and a `https://github.com/owner/repo/pull/number` URL. Rebinding is explicit; an agent's text cannot create bindings.
- Call `fetch(url, cwd).await` outside the database mutex, then `apply(q, job, &observation)` in a transaction. A changed binding rejects an old response. Request-start timestamps reject responses older than the last applied observation. `apply` returns `false` for a duplicate or stale response.
- `bindings(q, ticket)` supplies the UI's latest summary and pending count. Refresh is on demand by default; a future poller can use the same functions without gaining permission to wake jobs.
- `context(q, job)` supplies at most ten latest unconsumed observations under the current binding. Include them as untrusted external reference material in the next normally authorized input. Call `acknowledge(q, job, ids, turn)` in the transaction that commits a successful native reply, using only the IDs included in that turn's packet. Failed turns and process restarts keep feedback pending for retry. Repeating these reference facts does not replay tool calls. Do not acknowledge merely because the UI displayed them or an input was journaled.

Stopped, waiting, failed, completed and pending-approval jobs stay in their current state. This module creates no mailbox message or native steering call. A queued observation becomes useful when the normal scheduler/operator authorizes another turn; it is not an authorization itself.

## Evidence and boundaries

Observations retain the PR head SHA, merge/conflict state, aggregate CI state, individual check summaries and latest review summaries. Deduplication uses `(job, URL, head, digest)` over normalized, stably sorted summary contents, persists across restarts, and does not include the fetch timestamp. CI never treats empty/unknown checks or cancellation as success. Aggregation happens before truncating display lists.

The input process has a 20-second timeout and 1 MiB stdout limit. On failure it is killed/reaped; cancellation also enables `kill_on_drop`. Stderr is discarded, and errors do not include the command output or credentials. Args use `Command`, not a shell. Interactive prompts and stdin are disabled. Existing CLI authentication/environment is used without reading or persisting credentials.

Summary fields have character bounds, with at most 100 checks and 16 latest reviews. `partial` reports list truncation or missing arrays; a review body is capped at 800 characters. These are reference summaries, not a full PR audit: inline review threads, review resolution, diff content and branch protection requirements are not fetched. A change only beyond the stored summary bounds does not create a new digest. `mergeable=UNKNOWN` is retained as unknown. Enterprise GitHub, GitLab, arbitrary API hosts, auto-remediation and automatic job activation are outside this implementation.

Reference: [GitHub CLI `gh pr view`](https://cli.github.com/manual/gh_pr_view), checked 2026-11-09 with local `gh` 2.96.0. The queue and revision-based deduplication design was informed by [Agent Orchestrator SCM observer documentation](https://github.com/OrchestratorInc/agent-orchestrator/blob/main/docs/scm-observer.md); no upstream code was copied.

## Verification

`scripts/cargo.cmd test -p sa-engine --test scm` verifies real temporary SQLite journals, restart deduplication, head/content changes, ordering, stale response rejection, rebinding, job-scoped acknowledgements, unchanged job/approval/mailbox state, and bounded summaries. `scripts/cargo.cmd test -p sa-engine --test scm_delivery` verifies failed native execution, restart, manual retry with identical feedback, and consumption only after a successful reply. `scripts/cargo.cmd test -p sa-engine scm::tests` exercises strict URL/argv handling and real local subprocess success, stderr suppression, output limits and timeout cleanup. Tests do not contact GitHub or use live credentials. A live authenticated PR refresh and desktop acceptance remain separate runtime checks.

On 2026-11-09, the separate ignored `scm_live` test ran with `SA_LIVE_SCM_URL` set to this repository's existing PR #1: authenticated read-only fetch, durable observation and delivery to a fake adapter's authorized turn passed. No comment, push or merge was performed. This does not establish a real model's ability to fix CI or human desktop acceptance.
