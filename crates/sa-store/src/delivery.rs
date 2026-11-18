//! Read-only projection of versioned operator requirements, frozen files and
//! observed command results. An explicit assessment binds its input versions.
use crate::DB_FILE;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};
mod acceptance;
mod behavior;
pub use acceptance::{DeliveryAcceptance, acceptance_digest};
pub use behavior::{
    BEHAVIOR_ENVIRONMENT_POLICY, BEHAVIOR_RUNNER, BehaviorCheck, BehaviorReceipt,
    assessment_matches, behavior_basis, behavior_receipt,
};

pub const MAX_FILES: usize = 128;
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_REQUIREMENTS: usize = 256;
pub const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
pub const OBJECT_DIRECTORY: &str = "delivery-objects";
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS delivery_contracts(ticket INTEGER PRIMARY KEY REFERENCES tickets,revision INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS delivery_requirements(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets);
CREATE TABLE IF NOT EXISTS delivery_requirement_revisions(requirement INTEGER NOT NULL REFERENCES delivery_requirements,revision INTEGER NOT NULL,contract_revision INTEGER NOT NULL,text TEXT NOT NULL,path TEXT,expected_sha256 TEXT,job INTEGER REFERENCES jobs,source TEXT NOT NULL,created REAL NOT NULL,PRIMARY KEY(requirement,revision));
CREATE INDEX IF NOT EXISTS delivery_requirements_ticket ON delivery_requirements(ticket,id);
CREATE TABLE IF NOT EXISTS delivery_snapshots(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,contract_revision INTEGER NOT NULL,created REAL NOT NULL,payload TEXT NOT NULL,sha256 TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS delivery_snapshots_ticket ON delivery_snapshots(ticket,id);
CREATE TABLE IF NOT EXISTS delivery_requirement_dispositions(requirement INTEGER NOT NULL REFERENCES delivery_requirements,contract_revision INTEGER NOT NULL,required INTEGER NOT NULL,withdrawn INTEGER NOT NULL,reason TEXT NOT NULL,created REAL NOT NULL,PRIMARY KEY(requirement,contract_revision));
CREATE TABLE IF NOT EXISTS delivery_gate_policies(ticket INTEGER NOT NULL REFERENCES tickets,revision INTEGER NOT NULL,enabled INTEGER NOT NULL,created REAL NOT NULL,PRIMARY KEY(ticket,revision));
CREATE TABLE IF NOT EXISTS delivery_assessments(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,contract_revision INTEGER NOT NULL,snapshot_id INTEGER NOT NULL REFERENCES delivery_snapshots,policy_revision INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL,status TEXT NOT NULL,created REAL NOT NULL,unmet TEXT NOT NULL,reason TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS delivery_assessments_ticket ON delivery_assessments(ticket,id);
CREATE TABLE IF NOT EXISTS delivery_behavior_bindings(id INTEGER PRIMARY KEY,requirement INTEGER NOT NULL,requirement_revision INTEGER NOT NULL,contract_revision INTEGER NOT NULL,argv TEXT NOT NULL,verifier_files TEXT NOT NULL,created REAL NOT NULL,UNIQUE(requirement,contract_revision),FOREIGN KEY(requirement,requirement_revision) REFERENCES delivery_requirement_revisions(requirement,revision));
CREATE TABLE IF NOT EXISTS delivery_behavior_attempts(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,requirement INTEGER NOT NULL,requirement_revision INTEGER NOT NULL,binding_id INTEGER NOT NULL REFERENCES delivery_behavior_bindings,snapshot_id INTEGER NOT NULL REFERENCES delivery_snapshots,contract_revision INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL,started REAL NOT NULL,environment TEXT NOT NULL,FOREIGN KEY(requirement,requirement_revision) REFERENCES delivery_requirement_revisions(requirement,revision));
CREATE INDEX IF NOT EXISTS delivery_behavior_attempts_snapshot ON delivery_behavior_attempts(ticket,snapshot_id,requirement,id);
CREATE TABLE IF NOT EXISTS delivery_behavior_results(attempt_id INTEGER PRIMARY KEY REFERENCES delivery_behavior_attempts,status TEXT NOT NULL,exit_code INTEGER,detail TEXT NOT NULL,finished REAL NOT NULL);
CREATE TABLE IF NOT EXISTS delivery_assessment_behavior(assessment_id INTEGER PRIMARY KEY REFERENCES delivery_assessments,basis TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS delivery_acceptances(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL REFERENCES tickets,assessment_id INTEGER NOT NULL UNIQUE REFERENCES delivery_assessments,assessment_status TEXT NOT NULL,assessment_sha256 TEXT NOT NULL,created REAL NOT NULL);
CREATE INDEX IF NOT EXISTS delivery_acceptances_ticket ON delivery_acceptances(ticket,id);";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryEvidence {
    pub requirement_id: i64,
    pub requirement_revision: i64,
    pub status: String,
    pub reason: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequirementView {
    pub id: i64,
    pub revision: i64,
    pub text: String,
    pub path: Option<String>,
    pub expected_sha256: Option<String>,
    pub job: Option<i64>,
    pub source: String,
    pub status: String,
    pub evidence: Option<DeliveryEvidence>,
    #[serde(default = "default_required")]
    pub required: bool,
    #[serde(default)]
    pub withdrawn: bool,
    #[serde(default)]
    pub disposition_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<BehaviorCheck>,
}

fn default_required() -> bool {
    true
}

impl RequirementView {
    fn evidence_source(&self) -> &'static str {
        match (self.behavior.is_some(), self.path.is_some()) {
            (true, true) => "platform_combined",
            (true, false) => "platform_behavior",
            _ => "platform_sha256",
        }
    }

    fn same_definition(&self, other: &Self) -> bool {
        self.id == other.id
            && self.revision == other.revision
            && self.text == other.text
            && self.path == other.path
            && self.expected_sha256 == other.expected_sha256
            && self.job == other.job
            && self.source == other.source
            && self.required == other.required
            && self.withdrawn == other.withdrawn
            && self.disposition_reason == other.disposition_reason
            && behavior::same_binding(self.behavior.as_ref(), other.behavior.as_ref())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliverySnapshot {
    pub schema_version: u32,
    pub id: i64,
    pub ticket: i64,
    pub contract_revision: i64,
    pub created_at: f64,
    pub files: Vec<DeliveryFile>,
    pub requirements: Vec<RequirementView>,
    pub evidence: Vec<DeliveryEvidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliverySummary {
    pub ticket: i64,
    pub contract_revision: i64,
    pub requirements: Vec<RequirementView>,
    pub latest_snapshot: Option<DeliverySnapshot>,
    pub status: String,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub gate: DeliveryGate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryAssessment {
    pub id: i64,
    pub ticket: i64,
    pub contract_revision: i64,
    pub snapshot_id: i64,
    pub policy_revision: i64,
    pub manifest_sha256: String,
    pub status: String,
    pub created_at: f64,
    pub unmet: Vec<DeliveryEvidence>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryGate {
    pub enabled: bool,
    pub policy_revision: i64,
    pub latest_assessment: Option<DeliveryAssessment>,
    pub status: String,
    #[serde(default)]
    pub unmet: Vec<DeliveryEvidence>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub acceptance: Option<DeliveryAcceptance>,
    #[serde(default)]
    pub latest_acceptance: Option<DeliveryAcceptance>,
}

impl DeliveryGate {
    /// A partial conclusion can be accepted only with exactly the unmet
    /// criteria and evidence the operator is currently being shown.
    pub fn acceptable_assessment(&self) -> Option<&DeliveryAssessment> {
        self.latest_assessment.as_ref().filter(|assessment| {
            self.enabled
                && matches!(self.status.as_str(), "ready" | "partial")
                && self.policy_revision == assessment.policy_revision
                && self.status == assessment.status
                && self.unmet == assessment.unmet
        })
    }
}

impl Default for DeliveryGate {
    fn default() -> Self {
        Self {
            enabled: false,
            policy_revision: 0,
            latest_assessment: None,
            status: "disabled".into(),
            unmet: Vec::new(),
            reason: "交付门已关闭，需求证据保持旁路记录".into(),
            acceptance: None,
            latest_acceptance: None,
        }
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Platform-independent relative paths exclude Windows aliases and streams.
pub fn normalize_path(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= 1024,
        "交付文件路径为空或过长"
    );
    let normalized = value.replace('\\', "/");
    ensure!(
        !normalized.starts_with('/') && !normalized.contains(':') && !normalized.contains('\0'),
        "交付文件必须使用项目相对路径"
    );
    for part in normalized.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != ".." && !part.ends_with([' ', '.']),
            "交付文件路径含无效分量"
        );
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(base.len() == 4
                    && (base.starts_with("COM") || base.starts_with("LPT"))
                    && matches!(base.as_bytes()[3], b'1'..=b'9')),
            "交付文件路径含设备名称"
        );
        ensure!(
            !part
                .chars()
                .any(|ch| ch.is_control() || "<>\"|?*".contains(ch)),
            "交付文件路径含无效字符"
        );
    }
    ensure!(
        Path::new(&normalized)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "交付文件路径越界"
    );
    Ok(normalized)
}

fn linked(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Check every existing component, including the configured root's ancestors.
pub fn reject_links(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    for ancestor in absolute.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).context("无法读取交付路径属性")?;
        ensure!(!linked(&metadata), "交付路径含符号链接或重解析点");
    }
    Ok(())
}

pub fn project_file(root: &Path, path: &str) -> Result<PathBuf> {
    let path = normalize_path(path)?;
    reject_links(root)?;
    let canonical_root = root.canonicalize()?;
    let candidate = canonical_root.join(path);
    reject_links(&candidate)?;
    let canonical = candidate.canonicalize()?;
    ensure!(
        canonical.starts_with(&canonical_root) && canonical != canonical_root,
        "交付文件位于项目目录外"
    );
    ensure!(
        fs::symlink_metadata(&canonical)?.is_file(),
        "交付条目必须为普通文件"
    );
    Ok(candidate)
}

/// Bounded read and metadata/path checks before and after reading. Windows
/// denies write/delete sharing while the file is open and opens the leaf itself.
pub fn read_stable_file(path: &Path) -> Result<Vec<u8>> {
    read_file(path, false)
}

/// Projections share read/write/delete access, so viewing evidence cannot
/// temporarily prevent an executing agent from updating the project.
pub fn read_shared_file(path: &Path) -> Result<Vec<u8>> {
    read_file(path, true)
}

fn read_file(path: &Path, shared: bool) -> Result<Vec<u8>> {
    #[cfg(not(windows))]
    let _ = shared;
    reject_links(path)?;
    let canonical = path.canonicalize()?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .share_mode(if shared { 7 } else { 1 })
            .custom_flags(0x0020_0000);
    }
    let mut file = options.open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && !linked(&before) && before.len() <= MAX_FILE_BYTES,
        "交付文件类型无效或超过 4 MiB"
    );
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    reject_links(path)?;
    let current = fs::metadata(path)?;
    ensure!(
        path.canonicalize()? == canonical
            && bytes.len() as u64 == before.len()
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && current.len() == after.len()
            && current.modified()? == after.modified()?,
        "捕获期间文件发生变化"
    );
    Ok(bytes)
}

pub fn read_object(data_dir: &Path, file: &DeliveryFile) -> Result<Vec<u8>> {
    ensure!(
        valid_sha256(&file.sha256) && file.bytes <= MAX_FILE_BYTES,
        "交付对象声明无效"
    );
    let object = data_dir
        .join(OBJECT_DIRECTORY)
        .join(file.sha256.to_ascii_lowercase());
    let bytes = read_stable_file(&object)?;
    ensure!(
        bytes.len() as u64 == file.bytes && sha256(&bytes).eq_ignore_ascii_case(&file.sha256),
        "交付对象摘要或长度不符"
    );
    Ok(bytes)
}

fn open_readonly(data_dir: &Path) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(data_dir.join(DB_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    Ok(connection)
}

pub fn read_summary(data_dir: &Path, ticket: i64) -> Result<DeliverySummary> {
    let mut connection = open_readonly(data_dir)?;
    let transaction = connection.transaction()?;
    summary(&transaction, data_dir, ticket)
}

pub fn read_snapshot(data_dir: &Path, ticket: i64, snapshot_id: i64) -> Result<DeliverySnapshot> {
    let mut connection = open_readonly(data_dir)?;
    let transaction = connection.transaction()?;
    snapshot(&transaction, data_dir, ticket, snapshot_id)
}

/// Bind verification to the original manifest digest read by the caller.
/// Defaults added during deserialization never alter this byte identity.
pub fn read_snapshot_matching(
    data_dir: &Path,
    ticket: i64,
    snapshot_id: i64,
    expected_digest: &str,
) -> Result<DeliverySnapshot> {
    let mut connection = open_readonly(data_dir)?;
    let transaction = connection.transaction()?;
    snapshot_matching(
        &transaction,
        data_dir,
        ticket,
        snapshot_id,
        Some(expected_digest),
    )
}

/// An unlisted path is unknown. Finite manifests never prove file absence.
pub fn snapshot_file_sha256(
    data_dir: &Path,
    ticket: i64,
    snapshot_id: i64,
    path: &str,
) -> Result<String> {
    let frozen = read_snapshot(data_dir, ticket, snapshot_id)?;
    let normalized = normalize_path(path)?;
    let file = frozen
        .files
        .iter()
        .find(|file| file.path == normalized)
        .context("历史交付未捕获此文件，结果未知")?;
    read_object(data_dir, file)?;
    Ok(file.sha256.to_ascii_lowercase())
}

/// Recompute platform evidence from the frozen definition and verified file
/// manifest. Serialized result/status fields are never trusted as evidence.
pub fn evaluate_file_criterion(
    requirement: &RequirementView,
    files: &[DeliveryFile],
    invalid_paths: &HashSet<String>,
) -> DeliveryEvidence {
    let file = requirement
        .path
        .as_ref()
        .and_then(|path| files.iter().find(|file| &file.path == path));
    let (status, reason) = match (requirement.expected_sha256.as_ref(), file) {
        (Some(_), Some(file)) if invalid_paths.contains(&file.path) => {
            ("unknown", "历史交付对象缺失或损坏")
        }
        (Some(expected), Some(file)) if expected.eq_ignore_ascii_case(&file.sha256) => {
            ("pass", "平台捕获字节与操作员预先定义的 SHA-256 一致")
        }
        (Some(_), Some(_)) => ("fail", "平台捕获字节与预先定义的 SHA-256 不一致"),
        (Some(_), None) => ("unknown", "交付清单未捕获该需求文件"),
        _ => (
            "unknown",
            "尚未配置可执行的文件摘要标准，需求语义和用户验收待核查",
        ),
    };
    DeliveryEvidence {
        requirement_id: requirement.id,
        requirement_revision: requirement.revision,
        status: status.into(),
        reason: reason.into(),
        source: "platform_sha256".into(),
    }
}

pub fn requirements(connection: &Connection, ticket: i64) -> Result<Vec<RequirementView>> {
    let mut statement = connection.prepare("SELECT r.id,v.revision,v.text,v.path,v.expected_sha256,v.job,v.source FROM delivery_requirements r JOIN delivery_requirement_revisions v ON v.requirement=r.id WHERE r.ticket=? AND v.revision=(SELECT MAX(revision) FROM delivery_requirement_revisions WHERE requirement=r.id) ORDER BY r.id LIMIT 257")?;
    let mut rows = statement
        .query_map([ticket], |row| {
            Ok(RequirementView {
                id: row.get(0)?,
                revision: row.get(1)?,
                text: row.get(2)?,
                path: row.get(3)?,
                expected_sha256: row.get(4)?,
                job: row.get(5)?,
                source: row.get(6)?,
                status: "unknown".into(),
                evidence: None,
                required: true,
                withdrawn: false,
                disposition_reason: None,
                behavior: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len() <= MAX_REQUIREMENTS, "需求项数量超过限制");
    let has_dispositions = table_exists(connection, "delivery_requirement_dispositions")?;
    let has_bindings = table_exists(connection, "delivery_behavior_bindings")?;
    for requirement in &mut rows {
        if has_bindings {
            requirement.behavior = behavior::binding(connection, requirement.id, i64::MAX)?;
        }
        if let Some(expected) = &mut requirement.expected_sha256 {
            expected.make_ascii_lowercase();
        }
        if has_dispositions
            && let Some((required, withdrawn, reason)) = connection.query_row(
                "SELECT required,withdrawn,reason FROM delivery_requirement_dispositions WHERE requirement=? ORDER BY contract_revision DESC LIMIT 1",
                [requirement.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, String>(2)?)),
            ).optional()?
        {
            requirement.required = required;
            requirement.withdrawn = withdrawn;
            requirement.disposition_reason = Some(reason);
        }
    }
    Ok(rows)
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [name],
        |row| row.get(0),
    )?)
}

/// Metadata-only projection is safe inside a writer transaction. A previous
/// assessment stays historical until a read projection verifies its objects.
pub fn gate_metadata(connection: &Connection, ticket: i64) -> Result<DeliveryGate> {
    let mut gate = DeliveryGate::default();
    if !table_exists(connection, "delivery_gate_policies")? {
        return Ok(gate);
    }
    // A damaged revision destroys its high-water mark. Never guess a reusable
    // revision or silently fall back to an older enabled policy.
    let invalid_revision: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM delivery_gate_policies WHERE ticket=? AND (typeof(revision)!='integer' OR revision<=0))",
        [ticket],
        |row| row.get(0),
    )?;
    if invalid_revision {
        gate.policy_revision = -1;
        gate.status = "blocked".into();
        gate.reason = "交付门策略版本损坏，门已关闭。需求仍可编辑，请维护人员修复损坏的策略记录后再设置交付门".into();
    } else if let Some((revision, policy)) = connection.query_row(
        "SELECT revision,enabled,created FROM delivery_gate_policies WHERE ticket=? ORDER BY revision DESC LIMIT 1",
        [ticket],
        |row| {
            let revision = row.get::<_, i64>(0).ok();
            let policy = (|| -> Result<bool> {
                let enabled: i64 = row.get(1)?;
                let created: f64 = row.get(2)?;
                ensure!(matches!(enabled, 0 | 1) && created.is_finite() && created >= 0.0, "策略字段无效");
                Ok(enabled == 1)
            })();
            Ok((revision, policy))
        },
    ).optional()? {
        gate.policy_revision = revision.filter(|revision| *revision > 0).unwrap_or(-1);
        match policy {
            Ok(enabled) if gate.policy_revision > 0 => gate.enabled = enabled,
            _ => {
                gate.status = "blocked".into();
                gate.reason = "交付门策略记录损坏，门已关闭。有效版本号保留时可重新保存交付门设置以恢复".into();
            }
        }
    }
    if table_exists(connection, "delivery_assessments")? {
        let assessment = connection.query_row(
            "SELECT rowid,contract_revision,snapshot_id,policy_revision,manifest_sha256,status,created,unmet,reason FROM delivery_assessments WHERE ticket=? ORDER BY id DESC LIMIT 1",
            [ticket],
            |row| {
                let id = row.get(0)?;
                Ok(decode_assessment(row, ticket, id).unwrap_or_else(|_| DeliveryAssessment {
                    id, ticket, contract_revision: 0, snapshot_id: 0, policy_revision: 0,
                    manifest_sha256: String::new(), status: "invalid".into(), created_at: 0.0,
                    unmet: Vec::new(), reason: format!("历史交付评估 {id} 的字段、类型或内容损坏，请重新准备交付"),
                }))
            },
        ).optional()?;
        if let Some(assessment) = assessment {
            gate.latest_assessment = Some(assessment);
        }
        if table_exists(connection, "delivery_acceptances")? {
            gate.latest_acceptance = acceptance::latest(connection, ticket)?;
        }
    }
    if gate.enabled {
        gate.status = if gate.latest_assessment.is_some() {
            "stale"
        } else {
            "blocked"
        }
        .into();
        gate.reason = "交付门已启用，刷新可核验已记录的交付评估".into();
    }
    Ok(gate)
}

fn bounded_text(row: &Row<'_>, index: usize, max_bytes: usize) -> Result<String> {
    let ValueRef::Text(bytes) = row.get_ref(index)? else {
        anyhow::bail!("交付评估字段必须为文本");
    };
    ensure!(bytes.len() <= max_bytes, "交付评估字段超过限制");
    Ok(std::str::from_utf8(bytes)?.to_owned())
}

fn decode_assessment(row: &Row<'_>, ticket: i64, id: i64) -> Result<DeliveryAssessment> {
    let assessment = DeliveryAssessment {
        id,
        ticket,
        contract_revision: row.get(1)?,
        snapshot_id: row.get(2)?,
        policy_revision: row.get(3)?,
        manifest_sha256: bounded_text(row, 4, 64)?.to_ascii_lowercase(),
        status: bounded_text(row, 5, 16)?,
        created_at: row.get(6)?,
        unmet: serde_json::from_str(&bounded_text(row, 7, MAX_MANIFEST_BYTES)?)?,
        reason: bounded_text(row, 8, 8000)?,
    };
    ensure!(
        assessment.id > 0
            && assessment.contract_revision >= 0
            && assessment.snapshot_id > 0
            && assessment.policy_revision > 0,
        "交付评估身份无效"
    );
    ensure!(
        valid_sha256(&assessment.manifest_sha256)
            && assessment.created_at.is_finite()
            && assessment.created_at >= 0.0,
        "交付评估摘要或时间无效"
    );
    ensure!(
        matches!(assessment.status.as_str(), "ready" | "partial" | "blocked")
            && !assessment.reason.trim().is_empty(),
        "交付评估状态或理由无效"
    );
    ensure!(
        assessment.unmet.len() <= MAX_REQUIREMENTS
            && (assessment.status != "ready" || assessment.unmet.is_empty())
            && (assessment.status != "partial" || !assessment.unmet.is_empty()),
        "交付评估未满足项无效"
    );
    let mut identities = HashSet::new();
    for evidence in &assessment.unmet {
        ensure!(
            evidence.requirement_id > 0
                && evidence.requirement_revision > 0
                && identities.insert(evidence.requirement_id)
                && matches!(evidence.status.as_str(), "fail" | "unknown" | "stale")
                && matches!(
                    evidence.source.as_str(),
                    "platform_sha256" | "platform_behavior" | "platform_combined"
                )
                && !evidence.reason.trim().is_empty()
                && evidence.reason.len() <= 8000,
            "交付评估未满足项身份或内容无效"
        );
    }
    Ok(assessment)
}

/// Only active required criteria govern readiness. Evidence comes from a
/// verified frozen snapshot, while the authoritative contract supplies scope.
pub fn assess_snapshot(
    requirements: &[RequirementView],
    frozen: &DeliverySnapshot,
) -> (String, Vec<DeliveryEvidence>, String) {
    let mut unmet = Vec::new();
    let mut required_count = 0;
    let mut passed = 0;
    for requirement in requirements
        .iter()
        .filter(|item| item.required && !item.withdrawn)
    {
        required_count += 1;
        let evidence = frozen
            .requirements
            .iter()
            .find(|item| item.same_definition(requirement))
            .and_then(|item| item.evidence.clone())
            .unwrap_or_else(|| DeliveryEvidence {
                requirement_id: requirement.id,
                requirement_revision: requirement.revision,
                status: "unknown".into(),
                reason: "冻结清单未包含此版本的必需标准".into(),
                source: requirement.evidence_source().into(),
            });
        if evidence.status == "pass" {
            passed += 1;
        } else {
            unmet.push(evidence);
        }
    }
    let (status, reason) = if required_count == 0 {
        ("blocked", "没有有效的必需标准，无法准备交付")
    } else if passed == required_count {
        ("ready", "冻结快照的全部必需标准通过")
    } else if passed > 0 {
        ("partial", "冻结快照部分必需标准符合，其余标准见未满足项")
    } else {
        ("blocked", "冻结快照尚无必需标准符合，未满足项需要处理")
    };
    (status.into(), unmet, reason.into())
}

pub fn snapshot(
    connection: &Connection,
    data_dir: &Path,
    ticket: i64,
    snapshot_id: i64,
) -> Result<DeliverySnapshot> {
    snapshot_matching(connection, data_dir, ticket, snapshot_id, None)
}

fn snapshot_matching(
    connection: &Connection,
    data_dir: &Path,
    ticket: i64,
    snapshot_id: i64,
    expected_digest: Option<&str>,
) -> Result<DeliverySnapshot> {
    let (payload, digest, contract_revision): (String, String, i64) = connection
        .query_row(
            "SELECT payload,sha256,contract_revision FROM delivery_snapshots WHERE ticket=? AND id=?",
            [ticket, snapshot_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .context("交付快照不存在或不属于该工单")?;
    ensure!(
        payload.len() <= MAX_MANIFEST_BYTES
            && sha256(payload.as_bytes()).eq_ignore_ascii_case(&digest),
        "交付清单摘要不符或超过限制"
    );
    ensure!(
        expected_digest.is_none_or(|expected| expected.eq_ignore_ascii_case(&digest)),
        "核验期间交付清单摘要已改变，请重试"
    );
    let mut frozen: DeliverySnapshot = serde_json::from_str(&payload)?;
    ensure!(
        matches!(frozen.schema_version, 1 | 2)
            && frozen.ticket == ticket
            && frozen.id == snapshot_id
            && frozen.contract_revision == contract_revision
            && frozen.files.len() <= MAX_FILES
            && frozen.requirements.len() <= MAX_REQUIREMENTS
            && frozen.evidence.len() <= MAX_REQUIREMENTS,
        "交付清单身份或数量无效"
    );
    if frozen.schema_version == 2 {
        ensure!(
            frozen
                .requirements
                .iter()
                .any(|item| item.behavior.is_some()),
            "行为快照缺少采用的检查"
        );
    }
    let mut paths = HashSet::new();
    let mut invalid = HashSet::new();
    let mut total = 0;
    for file in &mut frozen.files {
        ensure!(
            normalize_path(&file.path)? == file.path
                && paths.insert(file.path.to_ascii_lowercase())
                && valid_sha256(&file.sha256),
            "交付清单路径或摘要重复或无效"
        );
        file.sha256.make_ascii_lowercase();
        ensure!(file.bytes <= MAX_FILE_BYTES, "交付文件超过体积限制");
        total += file.bytes;
        ensure!(total <= MAX_TOTAL_BYTES, "交付清单超过体积限制");
        if read_object(data_dir, file).is_err() {
            invalid.insert(file.path.clone());
        }
    }
    let mut requirement_ids = HashSet::new();
    let has_bindings = table_exists(connection, "delivery_behavior_bindings")?;
    let inputs = behavior::SnapshotInputs {
        ticket,
        snapshot_id,
        contract_revision,
        digest: &digest,
        files: &frozen.files,
        invalid: &invalid,
    };
    for requirement in &mut frozen.requirements {
        ensure!(
            requirement.id > 0
                && requirement.revision > 0
                && requirement_ids.insert(requirement.id)
                && requirement.source == "operator"
                && requirement.text.len() <= 8000,
            "交付需求身份或来源无效"
        );
        ensure!(
            requirement
                .disposition_reason
                .as_ref()
                .is_none_or(|reason| !reason.trim().is_empty() && reason.len() <= 8000),
            "交付需求处置理由无效"
        );
        ensure!(
            requirement.required && !requirement.withdrawn
                || requirement.disposition_reason.is_some(),
            "可选或撤销需求缺少操作员理由"
        );
        ensure!(
            requirement.path.is_some() == requirement.expected_sha256.is_some(),
            "交付需求摘要标准不完整"
        );
        if let Some(path) = &requirement.path {
            ensure!(normalize_path(path)? == *path, "交付需求路径无效");
        }
        if let Some(expected) = &mut requirement.expected_sha256 {
            ensure!(valid_sha256(expected), "交付需求摘要标准无效");
            expected.make_ascii_lowercase();
        }
        let binding = if has_bindings {
            behavior::binding(connection, requirement.id, contract_revision)?
        } else {
            None
        };
        ensure!(
            frozen.schema_version == 2 || binding.is_none(),
            "旧版快照未绑定行为标准"
        );
        ensure!(
            behavior::same_binding(binding.as_ref(), requirement.behavior.as_ref()),
            "行为标准与操作员采用的版本不符"
        );
        requirement.behavior = binding;
        let evidence = behavior::evidence(connection, requirement, &inputs)?;
        requirement.status = evidence.status.clone();
        requirement.evidence = Some(evidence);
    }
    frozen.evidence = frozen
        .requirements
        .iter()
        .filter_map(|requirement| requirement.evidence.clone())
        .collect();
    Ok(frozen)
}

pub fn summary(connection: &Connection, data_dir: &Path, ticket: i64) -> Result<DeliverySummary> {
    let project: String = connection
        .query_row("SELECT cwd FROM tickets WHERE id=?", [ticket], |row| {
            row.get(0)
        })
        .context("工单不存在")?;
    let mut result = DeliverySummary {
        ticket,
        contract_revision: 0,
        requirements: Vec::new(),
        latest_snapshot: None,
        status: "legacy_unassessed".into(),
        warning: None,
        gate: DeliveryGate::default(),
    };
    let has_schema: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='delivery_contracts')", [], |row| row.get(0))?;
    if !has_schema {
        return Ok(result);
    }
    result.contract_revision = connection
        .query_row(
            "SELECT revision FROM delivery_contracts WHERE ticket=?",
            [ticket],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    result.requirements = requirements(connection, ticket)?;
    result.gate = gate_metadata(connection, ticket)?;
    let snapshot_id = connection.query_row(
        "SELECT MAX(id) FROM delivery_snapshots WHERE ticket=?",
        [ticket],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    if let Some(id) = snapshot_id {
        match snapshot(connection, data_dir, ticket, id) {
            Ok(snapshot) => result.latest_snapshot = Some(snapshot),
            Err(error) => {
                result.warning = Some(format!(
                    "交付快照 {id} 无法核验，证据状态未知，可修订需求或重新捕获。{error}"
                ));
            }
        }
    }
    if result.gate.enabled
        && let Some(assessment) = &result.gate.latest_assessment
    {
        if assessment.status == "invalid" {
            result.gate.status = "blocked".into();
            result.gate.reason = assessment.reason.clone();
        } else if assessment.contract_revision != result.contract_revision
            || assessment.policy_revision != result.gate.policy_revision
            || Some(assessment.snapshot_id) != snapshot_id
        {
            result.gate.status = "stale".into();
            result.gate.reason = "需求合同、交付门设置或最新快照已改变，请重新准备交付".into();
        } else if let Some(frozen) = &result.latest_snapshot {
            let digest: String = connection.query_row(
                "SELECT sha256 FROM delivery_snapshots WHERE ticket=? AND id=?",
                [ticket, frozen.id],
                |row| row.get(0),
            )?;
            if !behavior::assessment_matches(connection, assessment).unwrap_or(false) {
                result.gate.status = "stale".into();
                result.gate.reason = "行为检查回执已改变或无法核验，请重新准备交付".into();
                let (_, unmet, _) = assess_snapshot(&result.requirements, frozen);
                result.gate.unmet = unmet;
            } else if digest.eq_ignore_ascii_case(&assessment.manifest_sha256) {
                let (status, unmet, reason) = assess_snapshot(&result.requirements, frozen);
                result.gate.unmet = unmet;
                if status == "ready" && assessment.status != "ready" {
                    result.gate.status = "stale".into();
                    result.gate.reason = "冻结对象现可核验，请重新准备交付以记录新评估".into();
                } else {
                    result.gate.status = status;
                    result.gate.reason = reason;
                }
            } else {
                result.gate.status = "stale".into();
                result.gate.reason = "交付清单摘要已改变，请重新准备交付".into();
            }
        } else {
            result.gate.status = "blocked".into();
            result.gate.reason = "冻结交付清单无法核验，请重新捕获并准备交付".into();
        }
    }
    if result.gate.enabled
        && matches!(result.gate.status.as_str(), "stale" | "blocked")
        && result.gate.unmet.is_empty()
    {
        if result.gate.latest_assessment.is_none() {
            result.gate.reason = "尚未记录交付评估，请选择当前快照并准备交付".into();
        }
        result.gate.unmet = result
            .requirements
            .iter()
            .filter(|item| item.required && !item.withdrawn)
            .map(|item| DeliveryEvidence {
                requirement_id: item.id,
                requirement_revision: item.revision,
                status: if result.gate.status == "stale" {
                    "stale"
                } else {
                    "unknown"
                }
                .into(),
                reason: result.gate.reason.clone(),
                source: item.evidence_source().into(),
            })
            .collect();
    }
    if let (Some(accepted), Some(assessment)) = (
        result.gate.latest_acceptance.as_ref(),
        result.gate.acceptable_assessment(),
    ) && accepted.assessment_id == assessment.id
    {
        result.gate.acceptance = Some(accepted.clone());
    }
    let mut current_files = std::collections::HashMap::new();
    let mut matches_current = |file: &DeliveryFile| -> bool {
        *current_files.entry(file.path.clone()).or_insert_with(|| {
            project_file(Path::new(&project), &file.path)
                .and_then(|path| read_shared_file(&path))
                .is_ok_and(|bytes| {
                    bytes.len() as u64 == file.bytes && sha256(&bytes) == file.sha256
                })
        })
    };
    for requirement in &mut result.requirements {
        let Some(frozen) = &result.latest_snapshot else {
            continue;
        };
        let Some(frozen_requirement) = frozen
            .requirements
            .iter()
            .find(|item| item.id == requirement.id)
        else {
            continue;
        };
        let Some(evidence) = &frozen_requirement.evidence else {
            continue;
        };
        let mut evidence = evidence.clone();
        if frozen.contract_revision != result.contract_revision
            || !requirement.same_definition(frozen_requirement)
        {
            evidence.status = "stale".into();
            evidence.reason = "需求合同已经修订".into();
        } else {
            requirement
                .behavior
                .clone_from(&frozen_requirement.behavior);
            let drifted = if requirement.behavior.is_some() {
                frozen.files.iter().any(|file| !matches_current(file))
            } else {
                evidence.status != "unknown"
                    && frozen.files.iter().any(|file| {
                        Some(&file.path) == requirement.path.as_ref() && !matches_current(file)
                    })
            };
            if drifted {
                evidence.status = "stale".into();
                evidence.reason = "当前项目输入文件与已捕获版本不一致或不可读取".into();
            }
        }
        requirement.status = evidence.status.clone();
        requirement.evidence = Some(evidence);
    }
    if !result.requirements.is_empty() {
        result.status = if result.requirements.iter().any(|item| item.status == "fail") {
            "fail"
        } else if result
            .requirements
            .iter()
            .any(|item| item.status == "stale")
        {
            "stale"
        } else if result.requirements.iter().all(|item| item.status == "pass") {
            "pass"
        } else {
            "unknown"
        }
        .into();
    } else if result.latest_snapshot.is_some() || result.warning.is_some() {
        result.status = "unknown".into();
    }
    Ok(result)
}
