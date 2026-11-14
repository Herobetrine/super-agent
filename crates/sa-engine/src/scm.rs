//! Operator-bound, read-only GitHub PR observations. External facts are queued,
//! never interpreted as permission to wake a job, approve a tool or publish code.

use crate::db::{Q, now, str_of, who_of};
use crate::protocol::digest;
use anyhow::{Result, anyhow, ensure};
use regex::Regex;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::LazyLock, time::Duration};
use tokio::io::AsyncReadExt;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS scm_bindings(
 job INTEGER PRIMARY KEY REFERENCES jobs(id),url TEXT NOT NULL,observed REAL NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS scm_observations(
 id INTEGER PRIMARY KEY,job INTEGER NOT NULL REFERENCES jobs(id),url TEXT NOT NULL,head TEXT NOT NULL,
 digest TEXT NOT NULL,summary TEXT NOT NULL,created REAL NOT NULL,consumed_turn INTEGER,
 UNIQUE(job,url,head,digest));
CREATE INDEX IF NOT EXISTS scm_observations_job ON scm_observations(job,id);";

const MAX_OUTPUT: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
const FIELDS: &str = "url,number,title,state,isDraft,headRefOid,baseRefName,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup,latestReviews";
static PR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^https://github\.com/([A-Za-z0-9][A-Za-z0-9-]{0,38})/([A-Za-z0-9_.-]{1,100})/pull/([1-9][0-9]{0,14})/?$"
).expect("valid PR pattern")
});

fn url(value: &str) -> Result<String> {
    let c = PR
        .captures(value)
        .ok_or_else(|| anyhow!("需要 https://github.com/owner/repo/pull/number 链接"))?;
    ensure!(!matches!(&c[2], "." | ".."), "GitHub 仓库名称无效");
    Ok(format!(
        "https://github.com/{}/{}/pull/{}",
        c[1].to_ascii_lowercase(),
        c[2].to_ascii_lowercase(),
        &c[3]
    ))
}

/// Must run inside the engine's transaction. Binding a PR is a local action;
/// ownership is explicit rather than inferred from an agent's output.
pub fn bind(q: &Q, job: i64, pr_url: &str) -> Result<()> {
    let pr_url = url(pr_url)?;
    let row = q
        .one("SELECT * FROM jobs WHERE id=?", &[&job])?
        .ok_or_else(|| anyhow!("任务不存在"))?;
    let previous = q.one("SELECT url FROM scm_bindings WHERE job=?", &[&job])?;
    if previous
        .as_ref()
        .is_some_and(|r| str_of(r, "url") == pr_url)
    {
        return Ok(());
    }
    q.run("INSERT INTO scm_bindings(job,url,observed) VALUES(?,?,0) ON CONFLICT(job) DO UPDATE SET url=excluded.url,observed=0", &[&job,&pr_url])?;
    q.log(who_of(&row), "scm.bound", &json!({"url":pr_url}))?;
    Ok(())
}

pub fn bindings(q: &Q, ticket: i64) -> Result<Vec<Value>> {
    let rows = q.all("SELECT b.job,b.url,b.observed FROM scm_bindings b JOIN jobs j ON j.id=b.job WHERE j.ticket=? ORDER BY b.job", &[&ticket])?;
    let mut out = vec![];
    for row in rows {
        let job = row["job"].as_i64().unwrap_or_default();
        let latest = q.one("SELECT id,summary,created FROM scm_observations WHERE job=? AND url=? ORDER BY id DESC LIMIT 1", &[&job,&str_of(&row,"url")])?;
        let pending = q.int("SELECT count(*) AS n FROM scm_observations WHERE job=? AND url=? AND consumed_turn IS NULL", &[&job,&str_of(&row,"url")])?;
        let mut value = Value::Object(row);
        value["pending"] = json!(pending);
        value["latest"] = match latest {
            Some(row) => {
                json!({"id":row["id"],"created":row["created"],"summary":serde_json::from_str::<Value>(str_of(&row,"summary"))?})
            }
            None => Value::Null,
        };
        out.push(value);
    }
    Ok(out)
}

fn gh_command(pr_url: &str, cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(["pr", "view", pr_url, "--json", FIELDS])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1");
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd
}

/// The only external command is `gh pr view URL --json ...`. No comments,
/// pushes, merges or credential output. gh's existing login is used as-is.
pub async fn fetch(pr_url: &str, cwd: &Path) -> Result<Value> {
    let pr_url = url(pr_url)?;
    let observed = now();
    let bytes = capture(gh_command(&pr_url, cwd), MAX_OUTPUT, TIMEOUT).await?;
    let mut data: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("GitHub CLI 未返回有效 JSON"))?;
    ensure!(data.is_object(), "GitHub CLI 返回格式无效");
    ensure!(
        url(data["url"].as_str().unwrap_or_default())? == pr_url,
        "GitHub 返回的 PR 与绑定不一致"
    );
    data["_sa_observed_at"] = json!(observed);
    Ok(data)
}

async fn capture(
    mut command: tokio::process::Command,
    max: u64,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut child = command
        .spawn()
        .map_err(|_| anyhow!("无法启动 gh，请安装 GitHub CLI 并完成登录"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法读取 GitHub CLI 输出"))?;
    let read = async {
        let mut bytes = vec![];
        stdout.take(max + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() as u64 <= max,
            "GitHub PR 数据超过输出上限，未保存不完整观察"
        );
        ensure!(
            child.wait().await?.success(),
            "无法读取 GitHub PR；请检查登录、仓库访问权限和网络"
        );
        Ok(bytes)
    };
    let result = tokio::time::timeout(timeout, read)
        .await
        .unwrap_or_else(|_| Err(anyhow!("读取 GitHub PR 超时")));
    if result.is_err() {
        // Reap the process on bounded-output/timeout failure. kill_on_drop also
        // covers cancellation by the caller while this future is suspended.
        let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
    }
    result
}

fn bounded(value: &Value, max: usize) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(max)
        .collect()
}

fn check_state(check: &Value) -> &'static str {
    let raw = check["conclusion"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| check["state"].as_str())
        .or_else(|| check["status"].as_str())
        .unwrap_or("UNKNOWN");
    match raw {
        "FAILURE" | "ERROR" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE" => {
            "failing"
        }
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => "passed",
        "CANCELLED" => "cancelled",
        "PENDING" | "IN_PROGRESS" | "QUEUED" | "REQUESTED" | "WAITING" => "pending",
        _ => "unknown",
    }
}

/// Keep a bounded summary and stable ordering for deduplication. Review bodies
/// are untrusted external reference text. Inline review threads are not fetched.
pub fn summarize(raw: &Value) -> Result<Value> {
    let pr_url = url(raw["url"].as_str().unwrap_or_default())?;
    let head = raw["headRefOid"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    ensure!(
        matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "GitHub PR 缺少有效 head SHA"
    );
    let checks_raw = raw["statusCheckRollup"].as_array();
    let mut checks: Vec<Value> = checks_raw.into_iter().flatten().map(|c| json!({
        "name":bounded(c.get("name").unwrap_or(&c["context"]),160),
        "state":check_state(c),"url":bounded(c.get("detailsUrl").unwrap_or(&c["targetUrl"]),1024),
        "completed_at":bounded(&c["completedAt"],40),
    })).collect();
    checks.sort_by_key(Value::to_string);
    checks.dedup();
    let check_count = checks.len();
    let has = |state: &str| checks.iter().any(|c| c["state"] == state);
    let ci = if has("failing") {
        "failing"
    } else if has("pending") {
        "pending"
    } else if has("cancelled") {
        "cancelled"
    } else if checks.is_empty() || has("unknown") {
        "unknown"
    } else {
        "passed"
    };
    checks.truncate(100);
    let reviews_raw = raw["latestReviews"].as_array();
    let mut reviews: Vec<Value> = reviews_raw
        .into_iter()
        .flatten()
        .map(|r| {
            json!({
                "id":bounded(&r["id"],160),"author":bounded(&r["author"]["login"],100),
                "state":bounded(&r["state"],40),"body":bounded(&r["body"],800),
                "url":bounded(&r["url"],1024),"submitted_at":bounded(&r["submittedAt"],40),
                "commit":bounded(&r["commit"]["oid"],64),
            })
        })
        .collect();
    reviews.sort_by_key(Value::to_string);
    reviews.dedup();
    let review_count = reviews.len();
    reviews.truncate(16);
    Ok(
        json!({"url":pr_url,"head":head,"title":bounded(&raw["title"],256),
        "state":bounded(&raw["state"],40),"draft":raw["isDraft"].as_bool(),
        "base":bounded(&raw["baseRefName"],256),"mergeable":bounded(&raw["mergeable"],40),
        "merge_state":bounded(&raw["mergeStateStatus"],40),"review_decision":bounded(&raw["reviewDecision"],40),
        "ci":ci,"checks":checks,"reviews":reviews,"check_count":check_count,"review_count":review_count,
        "partial":check_count>100 || review_count>16 || checks_raw.is_none() || reviews_raw.is_none(),
        "source":"github_untrusted","coverage":"PR checks and latest review summaries; inline review threads not fetched"}),
    )
}

/// Persist first, then expose through context on a later authorized turn. The
/// caller wraps this in `Db::tx`; no job state or permission is changed here.
pub fn apply(q: &Q, job: i64, raw: &Value) -> Result<bool> {
    let summary = summarize(raw)?;
    let binding = q
        .one("SELECT * FROM scm_bindings WHERE job=?", &[&job])?
        .ok_or_else(|| anyhow!("任务尚未绑定 PR"))?;
    let pr_url = summary["url"].as_str().unwrap_or_default();
    ensure!(str_of(&binding, "url") == pr_url, "PR 绑定在读取期间已更改");
    let observed = raw["_sa_observed_at"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or_else(now);
    if observed < binding["observed"].as_f64().unwrap_or_default() {
        return Ok(false);
    }
    let row = q
        .one("SELECT * FROM jobs WHERE id=?", &[&job])?
        .ok_or_else(|| anyhow!("任务不存在"))?;
    let body = summary.to_string();
    let sha = digest(&body);
    let head = summary["head"].as_str().unwrap_or_default();
    let added = q.run("INSERT OR IGNORE INTO scm_observations(job,url,head,digest,summary,created) VALUES(?,?,?,?,?,?)", &[&job,&pr_url,&head,&sha,&body,&observed])? > 0;
    q.run(
        "UPDATE scm_bindings SET observed=? WHERE job=? AND url=?",
        &[&observed, &job, &pr_url],
    )?;
    if added {
        q.log(
            who_of(&row),
            "scm.feedback",
            &json!({"url":pr_url,"head":head,"digest":sha,"ci":summary["ci"],"queued":true}),
        )?;
    }
    Ok(added)
}

/// Up to ten newest unconsumed observations. Include these in the envelope;
/// acknowledge only in the transaction committing a successful native reply.
/// Failed turns retain the same facts for retry, without replaying any tools.
pub fn context(q: &Q, job: i64) -> Result<Vec<Value>> {
    let rows = q.all("SELECT o.id,o.summary,o.created FROM scm_observations o JOIN scm_bindings b ON b.job=o.job AND b.url=o.url WHERE o.job=? AND o.consumed_turn IS NULL ORDER BY o.id DESC LIMIT 10", &[&job])?;
    rows.into_iter().map(|r| Ok(json!({"id":r["id"],"created":r["created"],"summary":serde_json::from_str::<Value>(str_of(&r,"summary"))?}))).collect()
}

pub fn acknowledge(q: &Q, job: i64, ids: &[i64], turn: i64) -> Result<()> {
    ensure!(ids.len() <= 10, "SCM 回执数量过多");
    for id in ids {
        q.run("UPDATE scm_observations SET consumed_turn=? WHERE job=? AND id=? AND consumed_turn IS NULL", &[&turn,&job,id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pr_urls_are_strict_and_command_is_read_only_argv() {
        assert_eq!(
            url("https://github.com/Owner/Repo/pull/42/").unwrap(),
            "https://github.com/owner/repo/pull/42"
        );
        for bad in [
            "https://github.com/owner/repo/issues/42",
            "https://github.com.evil/owner/repo/pull/42",
            "https://user@github.com/owner/repo/pull/42",
            "https://github.com/a/../pull/42",
            "https://github.com/a/b/pull/42?token=x",
            "--repo evil",
        ] {
            assert!(url(bad).is_err());
        }
        let cmd = gh_command("https://github.com/a/b/pull/1", Path::new("."));
        assert_eq!(
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "pr",
                "view",
                "https://github.com/a/b/pull/1",
                "--json",
                FIELDS
            ]
        );
    }

    // Real subprocess fixture: executes this test binary, not gh or a shell.
    #[test]
    fn process_fixture() {
        use std::io::Write;
        let Ok(mode) = std::env::var("SA_SCM_PROCESS_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "success" => {
                print!("fixture-output");
            }
            "large" => {
                let _ = std::io::stdout().write_all(&vec![b'x'; 16 * 1024]);
            }
            "sleep" => std::thread::sleep(Duration::from_secs(30)),
            "failure" => {
                eprintln!("secret-fixture-token");
                std::process::exit(1);
            }
            _ => panic!("unknown fixture mode"),
        }
    }

    fn fixture(mode: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "scm::tests::process_fixture", "--nocapture"])
            .env("SA_SCM_PROCESS_FIXTURE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd
    }

    #[tokio::test]
    async fn subprocess_capture_is_bounded_and_does_not_expose_stderr() {
        let output = capture(fixture("success"), 4096, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("fixture-output")
        );
        let error = capture(fixture("failure"), 4096, Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-fixture-token"));
        assert!(
            capture(fixture("large"), 4096, Duration::from_secs(5))
                .await
                .unwrap_err()
                .to_string()
                .contains("输出上限")
        );
        let started = std::time::Instant::now();
        assert!(
            capture(fixture("sleep"), 4096, Duration::from_millis(150))
                .await
                .unwrap_err()
                .to_string()
                .contains("超时")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
