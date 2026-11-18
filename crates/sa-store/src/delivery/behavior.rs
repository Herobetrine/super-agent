use super::*;

pub const BEHAVIOR_RUNNER: &str = "snapshot-argv/2";
pub const BEHAVIOR_ENVIRONMENT_POLICY: &str = "host-path-private-home-git-ceiling/2";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BehaviorCheck {
    pub id: i64,
    pub requirement_revision: i64,
    pub contract_revision: i64,
    pub argv: Vec<String>,
    pub verifier_files: Vec<DeliveryFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<BehaviorReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BehaviorReceipt {
    pub id: i64,
    pub ticket: i64,
    pub requirement: i64,
    pub requirement_revision: i64,
    pub binding_id: i64,
    pub snapshot_id: i64,
    pub contract_revision: i64,
    pub manifest_sha256: String,
    pub environment: String,
    pub started: f64,
    pub finished: Option<f64>,
    pub exit_code: Option<i32>,
    pub status: String,
    pub detail: String,
}

impl BehaviorCheck {
    pub fn same_definition(&self, other: &Self) -> bool {
        self.id == other.id
            && self.requirement_revision == other.requirement_revision
            && self.contract_revision == other.contract_revision
            && self.argv == other.argv
            && self.verifier_files == other.verifier_files
    }
}

pub(super) fn same_binding(left: Option<&BehaviorCheck>, right: Option<&BehaviorCheck>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_definition(right),
        _ => false,
    }
}

pub(super) fn binding(
    connection: &Connection,
    requirement: i64,
    at: i64,
) -> Result<Option<BehaviorCheck>> {
    Ok(connection.query_row(
        "SELECT id,requirement_revision,contract_revision,argv,verifier_files,created FROM delivery_behavior_bindings WHERE requirement=? AND contract_revision<=? ORDER BY contract_revision DESC LIMIT 1",
        [requirement, at], |row| {
            // Keep malformed adoptions as unmet criteria. A later explicit
            // adoption can repair them without rewriting this historical row.
            Ok(decode_binding(row).unwrap_or_else(|_| BehaviorCheck {
                id: 0, requirement_revision: 0, contract_revision: 0,
                argv: Vec::new(), verifier_files: Vec::new(), receipt: None,
            }))
        },
    ).optional()?)
}

fn decode_binding(row: &Row<'_>) -> Result<BehaviorCheck> {
    let binding = BehaviorCheck {
        id: row.get(0)?,
        requirement_revision: row.get(1)?,
        contract_revision: row.get(2)?,
        argv: serde_json::from_str(&bounded_text(row, 3, 16384)?)?,
        verifier_files: serde_json::from_str(&bounded_text(row, 4, MAX_MANIFEST_BYTES)?)?,
        receipt: None,
    };
    let created: f64 = row.get(5)?;
    ensure!(
        binding.id > 0
            && binding.requirement_revision > 0
            && binding.contract_revision > 0
            && created.is_finite()
            && created >= 0.0,
        "行为标准身份或时间无效"
    );
    ensure!(
        binding
            .argv
            .first()
            .is_some_and(|program| !program.trim().is_empty())
            && binding.argv.iter().all(|arg| !arg.contains('\0')),
        "行为标准命令无效"
    );
    ensure!(binding.verifier_files.len() <= MAX_FILES, "验证器文件过多");
    let mut paths = HashSet::new();
    let mut bytes = 0;
    for file in &binding.verifier_files {
        ensure!(
            normalize_path(&file.path)? == file.path
                && paths.insert(file.path.to_ascii_lowercase())
                && valid_sha256(&file.sha256)
                && file.bytes <= MAX_FILE_BYTES,
            "验证器文件声明无效"
        );
        bytes += file.bytes;
    }
    ensure!(bytes <= MAX_TOTAL_BYTES, "验证器文件超过总量限制");
    Ok(binding)
}

/// Read the append-only attempt and its optional final result. A running or
/// interrupted attempt has no passing result, including after a read-only reopen.
pub fn behavior_receipt(connection: &Connection, attempt_id: i64) -> Result<BehaviorReceipt> {
    connection.query_row(
        "SELECT a.id,a.ticket,a.requirement,a.requirement_revision,a.binding_id,a.snapshot_id,a.contract_revision,a.manifest_sha256,a.started,a.environment,r.attempt_id,r.status,r.exit_code,r.detail,r.finished FROM delivery_behavior_attempts a LEFT JOIN delivery_behavior_results r ON r.attempt_id=a.id WHERE a.id=?",
        [attempt_id], |row| Ok(decode_receipt(row)),
    )?
}

fn decode_receipt(row: &Row<'_>) -> Result<BehaviorReceipt> {
    let mut receipt = BehaviorReceipt {
        id: row.get(0)?,
        ticket: row.get(1)?,
        requirement: row.get(2)?,
        requirement_revision: row.get(3)?,
        binding_id: row.get(4)?,
        snapshot_id: row.get(5)?,
        contract_revision: row.get(6)?,
        manifest_sha256: bounded_text(row, 7, 64)?.to_ascii_lowercase(),
        started: row.get(8)?,
        environment: bounded_text(row, 9, 4096)?,
        finished: None,
        exit_code: None,
        status: "running".into(),
        detail: "尚未记录终态".into(),
    };
    ensure!(
        [
            receipt.id,
            receipt.ticket,
            receipt.requirement,
            receipt.requirement_revision,
            receipt.binding_id,
            receipt.snapshot_id,
            receipt.contract_revision
        ]
        .into_iter()
        .all(|id| id > 0)
            && valid_sha256(&receipt.manifest_sha256)
            && receipt.started.is_finite()
            && receipt.started >= 0.0
            && !receipt.environment.trim().is_empty(),
        "行为执行回执身份、时间或环境损坏"
    );
    if row.get::<_, Option<i64>>(10)?.is_some() {
        receipt.status = bounded_text(row, 11, 16)?;
        receipt.exit_code = row.get(12)?;
        receipt.detail = bounded_text(row, 13, 16384)?;
        let finished: f64 = row.get(14)?;
        ensure!(
            finished.is_finite()
                && finished >= receipt.started
                && matches!(receipt.status.as_str(), "pass" | "fail" | "invalid")
                && (receipt.status != "pass" || receipt.exit_code == Some(0)),
            "行为结果状态、退出码或时间无效"
        );
        receipt.finished = Some(finished);
    }
    Ok(receipt)
}

enum RunnerVersion {
    Current,
    Legacy,
}

fn verified_runner(receipt: &BehaviorReceipt, argv: &[String]) -> Option<RunnerVersion> {
    let env: serde_json::Value = serde_json::from_str(&receipt.environment).ok()?;
    let encoded = serde_json::to_vec(argv).ok()?;
    let version = match (env["runner"].as_str()?, env["environment_policy"].as_str()?) {
        (BEHAVIOR_RUNNER, BEHAVIOR_ENVIRONMENT_POLICY) => RunnerVersion::Current,
        ("snapshot-argv/1", "host-path-private-home/1") => RunnerVersion::Legacy,
        _ => return None,
    };
    let valid = env["cwd"] == "fresh_manifest_directory"
        && env["timeout_ms"]
            .as_u64()
            .is_some_and(|timeout| timeout > 0)
        && ["os", "arch", "program"].iter().all(|key| {
            env[key]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
        })
        && ["program_sha256", "path_sha256"]
            .iter()
            .all(|key| env[key].as_str().is_some_and(valid_sha256))
        && env["argv_sha256"] == sha256(&encoded);
    valid.then_some(version)
}

/// The exact latest attempt per requirement and whether its terminal record
/// exists. Starting a rerun and finishing it are separate assessment versions.
pub fn behavior_basis(connection: &Connection, ticket: i64, snapshot_id: i64) -> Result<String> {
    if !table_exists(connection, "delivery_behavior_attempts")? {
        return Ok(String::new());
    }
    let mut statement = connection.prepare(
        "SELECT a.requirement,a.id,EXISTS(SELECT 1 FROM delivery_behavior_results WHERE attempt_id=a.id)
         FROM (SELECT requirement,MAX(id) AS id FROM delivery_behavior_attempts WHERE ticket=? AND snapshot_id=? GROUP BY requirement ORDER BY requirement LIMIT 257) a ORDER BY a.requirement"
    )?;
    let attempts = statement
        .query_map([ticket, snapshot_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        attempts.len() <= MAX_REQUIREMENTS,
        "行为记录的需求数超过限制"
    );
    if attempts.is_empty() {
        Ok(String::new())
    } else {
        Ok(serde_json::to_string(&attempts)?)
    }
}

pub(super) fn saved_basis(connection: &Connection, assessment_id: i64) -> Result<String> {
    if !table_exists(connection, "delivery_assessment_behavior")? {
        return Ok(String::new());
    }
    Ok(connection
        .query_row(
            "SELECT basis FROM delivery_assessment_behavior WHERE assessment_id=?",
            [assessment_id],
            |row| Ok(bounded_text(row, 0, 32768)),
        )
        .optional()?
        .transpose()?
        .unwrap_or_default())
}

pub fn assessment_matches(
    connection: &Connection,
    assessment: &DeliveryAssessment,
) -> Result<bool> {
    let saved = saved_basis(connection, assessment.id)?;
    let current = behavior_basis(connection, assessment.ticket, assessment.snapshot_id)?;
    Ok(saved == current)
}

pub(super) struct SnapshotInputs<'a> {
    pub ticket: i64,
    pub snapshot_id: i64,
    pub contract_revision: i64,
    pub digest: &'a str,
    pub files: &'a [DeliveryFile],
    pub invalid: &'a HashSet<String>,
}

/// Called after verifying the manifest and its referenced objects. Receipt and
/// pass fields serialized in a manifest are never an authority for execution.
pub(super) fn evidence(
    connection: &Connection,
    requirement: &mut RequirementView,
    inputs: &SnapshotInputs<'_>,
) -> Result<DeliveryEvidence> {
    let mut file_evidence = evaluate_file_criterion(requirement, inputs.files, inputs.invalid);
    let Some(check) = &mut requirement.behavior else {
        return Ok(file_evidence);
    };
    let latest: Option<i64> = connection.query_row(
        "SELECT MAX(id) FROM delivery_behavior_attempts WHERE ticket=? AND snapshot_id=? AND requirement=?",
        [inputs.ticket, inputs.snapshot_id, requirement.id], |row| row.get(0),
    )?;
    check.receipt = None;
    let mut receipt_error = false;
    let mut legacy_pass = false;
    if let Some(id) = latest {
        match behavior_receipt(connection, id) {
            Ok(receipt)
                if receipt.requirement_revision == requirement.revision
                    && receipt.binding_id == check.id
                    && receipt.contract_revision == inputs.contract_revision
                    && receipt.manifest_sha256.eq_ignore_ascii_case(inputs.digest) =>
            {
                if receipt.status == "pass" {
                    match verified_runner(&receipt, &check.argv) {
                        Some(RunnerVersion::Current) => {}
                        Some(RunnerVersion::Legacy) => legacy_pass = true,
                        None => receipt_error = true,
                    }
                }
                if !receipt_error {
                    check.receipt = Some(receipt);
                }
            }
            _ => receipt_error = true,
        }
    }
    let (status, reason) = if check.id <= 0 {
        ("unknown", "采用的行为标准损坏，请重新采用")
    } else if check.requirement_revision != requirement.revision {
        ("stale", "需求已修订，请为此版本重新采用行为标准")
    } else if inputs.files.is_empty() || !inputs.invalid.is_empty() {
        ("unknown", "行为检查的快照输入缺失或损坏")
    } else if check
        .verifier_files
        .iter()
        .any(|pinned| !inputs.files.iter().any(|file| file == pinned))
    {
        ("stale", "快照未包含采用时的验证器版本")
    } else if receipt_error {
        ("unknown", "最新行为回执身份、版本或内容无法核验")
    } else if legacy_pass {
        (
            "stale",
            "历史检查使用旧执行环境策略，请在当前策略下重新运行",
        )
    } else {
        match check
            .receipt
            .as_ref()
            .map(|receipt| receipt.status.as_str())
        {
            Some("pass") => ("pass", "采用的行为检查在此快照上通过"),
            Some("fail") => ("fail", "采用的行为检查在此快照上失败"),
            Some("invalid") => ("unknown", "行为检查未产生有效证据"),
            Some("running") => ("unknown", "最新行为尝试尚无终态回执"),
            _ => ("unknown", "尚未在此快照运行采用的行为检查"),
        }
    };
    file_evidence.source = requirement.evidence_source().into();
    if requirement.path.is_some() && file_evidence.status != "pass" && status != "fail" {
        return Ok(file_evidence);
    }
    file_evidence.status = status.into();
    file_evidence.reason = if requirement.path.is_some() && status == "pass" {
        "文件摘要符合标准，采用的行为检查在此快照上通过".into()
    } else {
        reason.into()
    };
    Ok(file_evidence)
}
