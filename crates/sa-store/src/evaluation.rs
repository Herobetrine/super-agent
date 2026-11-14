//! Offline, read-only evaluation and privacy-preserving trace summaries.
//! Raw prompts, results, workspace paths, tool arguments and provider credentials
//! never enter a report. File assertions inspect the current workspace, not an
//! invented historical filesystem snapshot. No external telemetry is sent.

use crate::{DB_FILE, TicketUsage, usage};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path},
    time::Duration,
};

const MAX_JOBS: usize = 10_000;
const MAX_EVENTS: usize = 1_000_000;
const MAX_FILE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub cases: Vec<Case>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    /// Human-readable task specification, never copied to the report.
    pub task: String,
    pub assertions: Vec<Assertion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assertion {
    RootState { equals: String },
    ResultSha256 { equals: String },
    WorkspaceFileSha256 { path: String, equals: String },
    WorkspaceFileAbsent { path: String },
    MaxFailedJobs { maximum: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetIdentity {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub case_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssertionResult {
    pub index: usize,
    pub kind: String,
    /// `pass`, `fail`, or `unknown`; unknown is never counted as a pass.
    pub status: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Score {
    pub passed: usize,
    pub failed: usize,
    pub unknown: usize,
    /// Unknown evidence leaves the score absent, rather than inventing zero.
    pub fraction: Option<f64>,
    pub assertions: Vec<AssertionResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnTrace {
    pub job: i64,
    pub turn: i64,
    pub elapsed_ms: Option<u64>,
    pub terminal: bool,
    pub recoveries: u64,
    pub failures: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobTrace {
    pub job: i64,
    pub parent: Option<i64>,
    pub agent: String,
    pub state: String,
    pub turns: Vec<TurnTrace>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cost {
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub ticket: i64,
    pub through_event: i64,
    pub dataset: Option<DatasetIdentity>,
    pub score: Option<Score>,
    pub jobs: Vec<JobTrace>,
    pub failed_jobs: u64,
    pub recoveries: u64,
    /// Wall time from first admission to last terminal receipt; running or
    /// incomplete timing evidence keeps it unknown.
    pub elapsed_ms: Option<u64>,
    pub usage: TicketUsage,
    pub cost: Cost,
    pub privacy: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub schema_version: u32,
    pub dataset: Option<DatasetIdentity>,
    pub left_ticket: i64,
    pub right_ticket: i64,
    /// All deltas are right minus left. Missing/partial usage is not zero.
    pub score_delta: Option<f64>,
    pub elapsed_ms_delta: Option<i64>,
    pub input_tokens_delta: Option<i64>,
    pub output_tokens_delta: Option<i64>,
    pub failed_jobs_delta: i64,
    pub recoveries_delta: i64,
    pub cost_delta: Option<f64>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn state(value: &str) -> &str {
    match value {
        "queued" | "running" | "waiting" | "interrupted" | "done" | "partial" | "failed"
        | "cancelled" | "draining" => value,
        _ => "unknown",
    }
}
fn agent(value: &str) -> &str {
    match value {
        "codex" | "claude" | "deepseek" | "antigravity" | "acp" => value,
        _ => "other",
    }
}
fn terminal(value: &str) -> bool {
    matches!(value, "done" | "partial" | "failed" | "cancelled")
}

impl Dataset {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "不支持此数据集格式版本");
        ensure!(
            identifier(&self.id) && identifier(&self.version),
            "数据集标识或版本无效"
        );
        ensure!(
            !self.cases.is_empty() && self.cases.len() <= 1000,
            "数据集需要 1 到 1000 个 case"
        );
        let mut ids = std::collections::HashSet::new();
        for case in &self.cases {
            ensure!(
                identifier(&case.id) && ids.insert(&case.id),
                "case 标识无效或重复"
            );
            ensure!(
                !case.task.is_empty() && case.task.len() <= 64 * 1024,
                "case task 为空或过大"
            );
            ensure!(
                !case.assertions.is_empty() && case.assertions.len() <= 64,
                "case 需要 1 到 64 条断言"
            );
            for assertion in &case.assertions {
                match assertion {
                    Assertion::RootState { equals } => {
                        ensure!(terminal(equals), "只能断言已知终态")
                    }
                    Assertion::ResultSha256 { equals } => {
                        ensure!(valid_hash(equals), "断言 SHA-256 无效")
                    }
                    Assertion::WorkspaceFileSha256 { path, equals } => {
                        relative(path)?;
                        ensure!(valid_hash(equals), "断言 SHA-256 无效");
                    }
                    Assertion::WorkspaceFileAbsent { path } => relative(path)?,
                    Assertion::MaxFailedJobs { .. } => {}
                }
            }
        }
        Ok(())
    }
}

fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains(['\\', ':', '\0'])
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && path.split('/').all(|p| !p.is_empty()
                && p != "."
                && p != ".."
                && !p.eq_ignore_ascii_case(".git")
                && !p.ends_with(['.', ' '])),
        "文件断言必须使用安全的项目相对路径"
    );
    Ok(())
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Returns None only for a genuinely missing file; unsupported types and read
/// races are unknown evidence, not proof of absence.
fn workspace_hash(cwd: &str, path: &str) -> Result<Option<String>> {
    relative(path)?;
    let root = fs::canonicalize(cwd).context("项目目录不可用")?;
    let mut full = root.clone();
    for component in Path::new(path).components() {
        full.push(component.as_os_str());
        match fs::symlink_metadata(&full) {
            Ok(meta) => ensure!(!is_link(&meta), "文件断言不跟随链接或重解析点"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    let canonical = fs::canonicalize(&full)?;
    ensure!(canonical.starts_with(&root), "文件越出项目目录");
    let mut file = fs::File::open(&full)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= MAX_FILE,
        "文件不是普通文件或超过 64 MiB"
    );
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= MAX_FILE, "读取期间文件增长超限");
        hasher.update(&buf[..n]);
    }
    let after = fs::symlink_metadata(&full)?;
    ensure!(
        !is_link(&after)
            && after.is_file()
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && count == after.len()
            && fs::canonicalize(&full)? == canonical,
        "评测期间文件改变"
    );
    Ok(Some(format!("{:x}", hasher.finalize())))
}

#[derive(Default)]
struct Timing {
    start: Option<f64>,
    end: Option<f64>,
    recoveries: u64,
    failures: u64,
    invalid: bool,
}
fn duration(start: Option<f64>, end: Option<f64>) -> Option<u64> {
    start.zip(end).and_then(|(a, b)| {
        let ms = (b - a) * 1000.0;
        (a.is_finite() && b.is_finite() && a >= 0.0 && b >= a && ms < u64::MAX as f64)
            .then(|| ms.round() as u64)
    })
}

/// One consistent read-only SQLite snapshot, with no scheduler or model calls.
pub fn evaluate(
    data_dir: &Path,
    ticket: i64,
    dataset_case: Option<(&Dataset, &str)>,
) -> Result<Report> {
    ensure!(ticket > 0, "工单编号无效");
    let selected = if let Some((dataset, id)) = dataset_case {
        dataset.validate()?;
        Some((
            dataset,
            dataset
                .cases
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow::anyhow!("数据集不含指定 case"))?,
        ))
    } else {
        None
    };
    let conn = Connection::open_with_flags(
        data_dir.join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
    let cwd: String = conn
        .query_row("SELECT cwd FROM tickets WHERE id=?", [ticket], |r| r.get(0))
        .optional()?
        .context("工单不存在")?;
    let end: i64 = conn.query_row(
        "SELECT coalesce(max(seq),0) FROM events WHERE ticket=?",
        [ticket],
        |r| r.get(0),
    )?;
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM events WHERE ticket=? AND seq<=?",
        [ticket, end],
        |r| r.get(0),
    )?;
    ensure!(count <= MAX_EVENTS as i64, "工单超过评测事件上限");
    let mut stmt = conn
        .prepare("SELECT id,parent,agent,state FROM jobs WHERE ticket=? ORDER BY id LIMIT 10001")?;
    let rows = stmt
        .query_map([ticket], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        !rows.is_empty() && rows.len() <= MAX_JOBS,
        "工单任务数为空或超限"
    );
    let root = rows.iter().find(|r| r.1.is_none()).context("缺少主任务")?;
    let root_state = root.3.clone();
    let root_result: Option<String> = conn.query_row("SELECT CASE WHEN length(CAST(result AS BLOB))<=67108864 THEN result END FROM jobs WHERE id=?", [root.0], |r|r.get(0))?;
    let mut timings: BTreeMap<(i64, i64), Timing> = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT job,turn,ts,kind FROM events WHERE ticket=? AND seq<=? AND job IS NOT NULL AND turn IS NOT NULL AND kind IN ('turn.admitted','reply','failed','cancelled','interrupted','turn.retry_scheduled','turn.gave_up','route.rotated','turn.route_wait','auto.retry') ORDER BY seq")?;
    let mut events = stmt.query([ticket, end])?;
    while let Some(row) = events.next()? {
        let key = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?);
        let t = timings.entry(key).or_default();
        let time: f64 = row.get(2)?;
        let kind: String = row.get(3)?;
        if !time.is_finite() || time < 0.0 {
            t.invalid = true;
            continue;
        }
        if kind == "turn.admitted" {
            if t.start.is_none() {
                t.start = Some(time);
            }
        } else {
            if t.end.is_none() {
                t.end = Some(time);
            }
            if matches!(
                kind.as_str(),
                "turn.retry_scheduled" | "route.rotated" | "turn.route_wait" | "auto.retry"
            ) {
                t.recoveries += 1;
            }
            if matches!(kind.as_str(), "failed" | "turn.gave_up" | "interrupted") {
                t.failures += 1;
            }
        }
    }
    let visible: Vec<_> = rows.iter().map(|r| (r.0, r.2.clone())).collect();
    let mut projection = usage::Entry::default();
    let mut usage = projection.view(end, &visible, rows.len() as i64);
    while usage.pending {
        projection.scan(&conn, ticket, end)?;
        usage = projection.view(end, &visible, rows.len() as i64);
    }
    // Only allowlisted execution identities cross into an exported report.
    for job in &mut usage.jobs {
        job.agent = agent(&job.agent).into();
    }
    let failed_jobs = rows.iter().filter(|r| r.3 == "failed").count() as u64;
    let recoveries = timings.values().map(|t| t.recoveries).sum();
    let complete = rows
        .iter()
        .all(|r| terminal(&r.3) && timings.keys().any(|(job, _)| *job == r.0))
        && !timings.is_empty()
        && timings
            .values()
            .all(|t| !t.invalid && duration(t.start, t.end).is_some());
    let start = timings
        .values()
        .filter_map(|t| t.start)
        .min_by(f64::total_cmp);
    let finish = timings
        .values()
        .filter_map(|t| t.end)
        .max_by(f64::total_cmp);
    let elapsed_ms = complete.then(|| duration(start, finish)).flatten();
    let jobs = rows
        .iter()
        .map(|r| JobTrace {
            job: r.0,
            parent: r.1,
            agent: agent(&r.2).into(),
            state: state(&r.3).into(),
            turns: timings
                .iter()
                .filter(|((job, _), _)| *job == r.0)
                .map(|((job, turn), t)| TurnTrace {
                    job: *job,
                    turn: *turn,
                    elapsed_ms: (!t.invalid).then(|| duration(t.start, t.end)).flatten(),
                    terminal: t.end.is_some(),
                    recoveries: t.recoveries,
                    failures: t.failures,
                })
                .collect(),
        })
        .collect();
    let (identity, score) = if let Some((dataset, case)) = selected {
        let assertions: Vec<_> = case
            .assertions
            .iter()
            .enumerate()
            .map(|(index, assertion)| {
                let (kind, evidence, outcome): (&str, &str, Option<bool>) = match assertion {
                    Assertion::RootState { equals } => {
                        ("root_state", "journal_state", Some(&root_state == equals))
                    }
                    Assertion::ResultSha256 { equals } => (
                        "result_sha256",
                        "recorded_output",
                        root_result
                            .as_ref()
                            .map(|s| digest(s.as_bytes()) == *equals),
                    ),
                    Assertion::WorkspaceFileSha256 { path, equals } => (
                        "workspace_file_sha256",
                        "current_workspace",
                        workspace_hash(&cwd, path)
                            .ok()
                            .map(|hash| hash.as_deref() == Some(equals)),
                    ),
                    Assertion::WorkspaceFileAbsent { path } => (
                        "workspace_file_absent",
                        "current_workspace",
                        workspace_hash(&cwd, path).ok().map(|hash| hash.is_none()),
                    ),
                    Assertion::MaxFailedJobs { maximum } => (
                        "max_failed_jobs",
                        "journal_state",
                        Some(failed_jobs <= *maximum),
                    ),
                };
                AssertionResult {
                    index,
                    kind: kind.into(),
                    evidence: evidence.into(),
                    status: match outcome {
                        Some(true) => "pass",
                        Some(false) => "fail",
                        None => "unknown",
                    }
                    .into(),
                }
            })
            .collect();
        let passed = assertions.iter().filter(|a| a.status == "pass").count();
        let failed = assertions.iter().filter(|a| a.status == "fail").count();
        let unknown = assertions.len() - passed - failed;
        (
            Some(DatasetIdentity {
                id: dataset.id.clone(),
                version: dataset.version.clone(),
                sha256: digest(&serde_json::to_vec(dataset)?),
                case_id: case.id.clone(),
            }),
            Some(Score {
                passed,
                failed,
                unknown,
                fraction: (unknown == 0).then_some(passed as f64 / assertions.len() as f64),
                assertions,
            }),
        )
    } else {
        (None, None)
    };
    conn.execute_batch("ROLLBACK")?;
    Ok(Report {schema_version:1,ticket,through_event:end,dataset:identity,score,jobs,failed_jobs,recoveries,elapsed_ms,usage,
        cost:Cost{amount:None,currency:None,reason:"price_and_billing_not_available".into()},
        privacy:"No prompts, results, paths, tool arguments, provider endpoints or credentials exported; dataset identifiers are operator supplied.".into()})
}

fn delta(left: Option<u64>, right: Option<u64>) -> Option<i64> {
    left.zip(right)
        .and_then(|(a, b)| i64::try_from(i128::from(b) - i128::from(a)).ok())
}

pub fn compare(left: &Report, right: &Report) -> Result<Comparison> {
    ensure!(
        left.schema_version == 1 && right.schema_version == 1,
        "报告格式版本不兼容"
    );
    ensure!(
        left.dataset == right.dataset,
        "不同数据集、版本、case 或断言内容不能直接比较"
    );
    let known_usage =
        !left.usage.partial && !right.usage.partial && !left.usage.pending && !right.usage.pending;
    Ok(Comparison {
        schema_version: 1,
        dataset: left.dataset.clone(),
        left_ticket: left.ticket,
        right_ticket: right.ticket,
        score_delta: left
            .score
            .as_ref()
            .and_then(|s| s.fraction)
            .zip(right.score.as_ref().and_then(|s| s.fraction))
            .map(|(a, b)| b - a),
        elapsed_ms_delta: delta(left.elapsed_ms, right.elapsed_ms),
        input_tokens_delta: known_usage
            .then(|| {
                delta(
                    left.usage.totals.input_tokens,
                    right.usage.totals.input_tokens,
                )
            })
            .flatten(),
        output_tokens_delta: known_usage
            .then(|| {
                delta(
                    left.usage.totals.output_tokens,
                    right.usage.totals.output_tokens,
                )
            })
            .flatten(),
        failed_jobs_delta: delta(Some(left.failed_jobs), Some(right.failed_jobs))
            .context("失败数量超过可比较范围")?,
        recoveries_delta: delta(Some(left.recoveries), Some(right.recoveries))
            .context("恢复数量超过可比较范围")?,
        cost_delta: None,
    })
}

/// Export only to a new file; never replace an existing report or database.
pub fn write_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    out.write_all(&bytes)?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    Ok(())
}
