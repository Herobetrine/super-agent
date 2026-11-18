//! Operator-owned requirements, explicit snapshots and delivery checks.
mod acceptance;
mod behavior;
use crate::{
    Engine,
    db::{Q, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use sa_store::delivery::{
    self, DeliveryAssessment, DeliveryFile, DeliverySnapshot, DeliverySummary,
};
use serde_json::json;
use std::{collections::HashSet, fs, io::Write, path::Path};

fn ticket_project(q: &Q, ticket: i64) -> Result<String> {
    ensure!(ticket > 0, "工单编号无效");
    q.conn()
        .query_row("SELECT cwd FROM tickets WHERE id=?", [ticket], |row| {
            row.get(0)
        })
        .context("工单不存在")
}

fn contract_revision(q: &Q, ticket: i64) -> Result<i64> {
    Ok(q.conn()
        .query_row(
            "SELECT revision FROM delivery_contracts WHERE ticket=?",
            [ticket],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn event(q: &Q, ticket: i64, kind: &str, payload: serde_json::Value) -> Result<()> {
    let payload = serde_json::to_string(&payload)?;
    q.run(
        "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,NULL,NULL,?,?,?)",
        &[
            &now(),
            &ticket,
            &kind,
            &payload,
            &delivery::sha256(payload.as_bytes()),
        ],
    )?;
    Ok(())
}

fn committed_summary(q: &Q, ticket: i64, revision: i64) -> Result<DeliverySummary> {
    Ok(DeliverySummary {
        ticket,
        contract_revision: revision,
        requirements: delivery::requirements(q.conn(), ticket)?,
        latest_snapshot: None,
        status: "unknown".into(),
        warning: Some("操作已保存，刷新可重新核验交付证据".into()),
        gate: delivery::gate_metadata(q.conn(), ticket)?,
    })
}

fn publish_object(data_dir: &Path, file: &DeliveryFile, bytes: &[u8]) -> Result<()> {
    delivery::reject_links(data_dir)?;
    let directory = data_dir.join(delivery::OBJECT_DIRECTORY);
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    delivery::reject_links(&directory)?;
    ensure!(directory.is_dir(), "交付对象目录无效");
    let target = directory.join(&file.sha256);
    if target.try_exists()? {
        return repair_existing_object(data_dir, file, bytes);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    ensure!(
        delivery::sha256(&fs::read(temporary.path())?) == file.sha256,
        "临时交付对象摘要不符"
    );
    delivery::reject_links(&directory)?;
    match temporary.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return repair_existing_object(data_dir, file, bytes);
        }
        Err(error) => return Err(error.error.into()),
    }
    delivery::read_object(data_dir, file)?;
    Ok(())
}

/// Only a successful bounded stable read can establish corruption. Permission,
/// sharing, link and type failures are propagated before any repair is written.
fn repair_existing_object(data_dir: &Path, file: &DeliveryFile, correct: &[u8]) -> Result<()> {
    let directory = data_dir.join(delivery::OBJECT_DIRECTORY);
    let target = directory.join(&file.sha256);
    delivery::reject_links(&target)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny concurrent writes while validating and preserving damaged bytes.
        options.share_mode(5).custom_flags(0x0020_0000);
    }
    let object_guard = options.open(&target).context("无法锁定待核验的交付对象")?;
    let damaged = delivery::read_stable_file(&target).context("无法稳定读取待核验的交付对象")?;
    if damaged.len() as u64 == file.bytes && delivery::sha256(&damaged) == file.sha256 {
        return Ok(());
    }
    ensure!(
        correct.len() as u64 == file.bytes && delivery::sha256(correct) == file.sha256,
        "修复交付对象的输入摘要无效"
    );
    let quarantine = directory.join("quarantine");
    match fs::create_dir(&quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    delivery::reject_links(&quarantine)?;
    ensure!(quarantine.is_dir(), "交付对象隔离目录无效");
    let prefix = format!("{}.{}.", file.sha256, delivery::sha256(&damaged));
    let mut backup = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".damaged")
        .tempfile_in(&quarantine)?;
    backup.write_all(&damaged)?;
    backup.as_file().sync_all()?;
    backup
        .keep()
        .map_err(|error| error.error)
        .context("无法保留损坏交付对象的隔离备份")?;
    let mut repaired = tempfile::NamedTempFile::new_in(&directory)?;
    repaired.write_all(correct)?;
    repaired.as_file().sync_all()?;
    ensure!(
        delivery::sha256(&fs::read(repaired.path())?) == file.sha256,
        "修复交付对象的临时摘要无效"
    );
    // Windows MoveFileEx replacement requires the destination handle closed.
    // Keep the validated bytes guarded until the replacement is fully ready.
    drop(object_guard);
    delivery::reject_links(&target)?;
    // Persist replaces the leaf atomically. The original damaged bytes have
    // already been durably saved under a unique name in the quarantine.
    repaired
        .persist(&target)
        .map_err(|error| error.error)
        .context("无法原子替换损坏的交付对象")?;
    delivery::read_object(data_dir, file)?;
    Ok(())
}

impl Engine {
    pub fn delivery_summary(&self, ticket: i64) -> Result<DeliverySummary> {
        delivery::read_summary(self.data_dir(), ticket)
    }

    /// Operator-only policy. This appends a revision and never changes jobs,
    /// continuous execution, scheduler admission or model-call policy.
    pub fn set_delivery_gate(
        &self,
        ticket: i64,
        expected_policy_revision: i64,
        enabled: bool,
    ) -> Result<DeliverySummary> {
        self.db().tx(|q| {
            ticket_project(q, ticket)?;
            let gate = delivery::gate_metadata(q.conn(), ticket)?;
            ensure!(gate.policy_revision >= 0, "交付门策略版本损坏，请维护人员修复策略记录后重试");
            ensure!(gate.policy_revision == expected_policy_revision, "交付门设置已被修改，请刷新后重试");
            let revision = gate.policy_revision.checked_add(1).context("交付门版本已超出限制")?;
            q.run("INSERT INTO delivery_gate_policies(ticket,revision,enabled,created) VALUES(?,?,?,?)", &[&ticket, &revision, &enabled, &now()])?;
            event(q, ticket, "delivery.gate_changed", json!({"schema_version":1,"policy_revision":revision,"enabled":enabled,"source":"operator"}))?;
            committed_summary(q, ticket, contract_revision(q, ticket)?)
        })
    }

    pub fn set_requirement_disposition(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        requirement_id: i64,
        required: bool,
        withdrawn: bool,
        reason: String,
    ) -> Result<DeliverySummary> {
        let reason = reason.trim();
        ensure!(
            !reason.is_empty() && reason.len() <= 8000,
            "需求处置必须填写理由且最多 8000 字节"
        );
        self.db().tx(|q| {
            ticket_project(q, ticket)?;
            let current = contract_revision(q, ticket)?;
            ensure!(current == expected_contract_revision, "需求合同已被修改，请刷新后重试");
            ensure!(q.exists("SELECT 1 FROM delivery_requirements WHERE ticket=? AND id=?", &[&ticket, &requirement_id])?, "需求不属于该工单");
            let revision = current.checked_add(1).context("需求合同版本已超出限制")?;
            q.run("INSERT INTO delivery_requirement_dispositions(requirement,contract_revision,required,withdrawn,reason,created) VALUES(?,?,?,?,?,?)", &[&requirement_id, &revision, &required, &withdrawn, &reason, &now()])?;
            q.run("UPDATE delivery_contracts SET revision=? WHERE ticket=?", &[&revision, &ticket])?;
            event(q, ticket, "requirement.disposition_changed", json!({"schema_version":1,"requirement_id":requirement_id,"contract_revision":revision,"required":required,"withdrawn":withdrawn,"reason":reason,"source":"operator"}))?;
            committed_summary(q, ticket, revision)
        })
    }

    /// Verify the explicit immutable snapshot before acquiring a writer
    /// transaction, then CAS every referenced revision and manifest digest.
    /// External object mutation is detected by every later read projection.
    pub fn prepare_delivery(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        expected_snapshot_id: i64,
        expected_policy_revision: i64,
    ) -> Result<DeliveryAssessment> {
        self.prepare_delivery_inner(
            ticket,
            expected_contract_revision,
            expected_snapshot_id,
            expected_policy_revision,
            || Ok(()),
        )
    }

    fn prepare_delivery_inner(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        expected_snapshot_id: i64,
        expected_policy_revision: i64,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<DeliveryAssessment> {
        let (requirements, manifest_sha256, behavior_basis) = self.db().with(|q| {
            ticket_project(q, ticket)?;
            let gate = delivery::gate_metadata(q.conn(), ticket)?;
            ensure!(
                gate.enabled && gate.policy_revision == expected_policy_revision,
                "交付门未启用或设置已改变"
            );
            let digest = behavior::current_manifest(
                q,
                ticket,
                expected_contract_revision,
                expected_snapshot_id,
            )?;
            Ok((
                delivery::requirements(q.conn(), ticket)?,
                digest,
                delivery::behavior_basis(q.conn(), ticket, expected_snapshot_id)?,
            ))
        })?;
        let frozen = delivery::read_snapshot_matching(
            self.data_dir(),
            ticket,
            expected_snapshot_id,
            &manifest_sha256,
        )?;
        ensure!(
            frozen.contract_revision == expected_contract_revision,
            "交付快照需求版本已过期"
        );
        let (status, unmet, reason) = delivery::assess_snapshot(&requirements, &frozen);
        let mut assessment = DeliveryAssessment {
            id: 0,
            ticket,
            contract_revision: expected_contract_revision,
            snapshot_id: expected_snapshot_id,
            policy_revision: expected_policy_revision,
            manifest_sha256,
            status,
            created_at: now(),
            unmet,
            reason,
        };
        let unmet = serde_json::to_string(&assessment.unmet)?;
        before_commit()?;
        self.db().tx(|q| {
            ticket_project(q, ticket)?;
            let gate = delivery::gate_metadata(q.conn(), ticket)?;
            ensure!(gate.enabled && gate.policy_revision == expected_policy_revision, "核验期间交付门设置已改变，请重试");
            ensure!(behavior::current_manifest(q, ticket, expected_contract_revision, expected_snapshot_id)? == assessment.manifest_sha256, "核验期间交付清单已改变，请重试");
            ensure!(delivery::behavior_basis(q.conn(), ticket, expected_snapshot_id)? == behavior_basis, "核验期间行为检查回执已改变，请重试");
            assessment.id = q.insert("INSERT INTO delivery_assessments(ticket,contract_revision,snapshot_id,policy_revision,manifest_sha256,status,created,unmet,reason) VALUES(?,?,?,?,?,?,?,?,?)", &[&ticket, &expected_contract_revision, &expected_snapshot_id, &expected_policy_revision, &assessment.manifest_sha256, &assessment.status, &assessment.created_at, &unmet, &assessment.reason])?;
            q.run("INSERT INTO delivery_assessment_behavior(assessment_id,basis) VALUES(?,?)", &[&assessment.id,&behavior_basis])?;
            event(q, ticket, "delivery.assessed", json!({"schema_version":2,"assessment_id":assessment.id,"contract_revision":expected_contract_revision,"snapshot_id":expected_snapshot_id,"policy_revision":expected_policy_revision,"manifest_sha256":assessment.manifest_sha256,"behavior_basis":behavior_basis,"status":assessment.status,"source":"platform_delivery"}))?;
            Ok(())
        })?;
        Ok(assessment)
    }

    /// Explicit operator edits only. Revision CAS covers the whole contract,
    /// including additions, so simultaneous editors cannot overwrite intent.
    // Keep the explicit operator-edit API shared with the desktop action.
    #[allow(clippy::too_many_arguments)]
    pub fn save_requirement(
        &self,
        ticket: i64,
        expected_revision: i64,
        requirement_id: Option<i64>,
        text: String,
        path: Option<String>,
        expected_sha256: Option<String>,
        job: Option<i64>,
    ) -> Result<DeliverySummary> {
        let text = text.trim();
        ensure!(
            !text.is_empty() && text.len() <= 8000,
            "需求内容不能为空且最多 8000 字节"
        );
        let path = path.as_deref().map(delivery::normalize_path).transpose()?;
        ensure!(
            path.is_some() == expected_sha256.is_some(),
            "文件摘要标准必须同时提供路径和 SHA-256"
        );
        let expected_sha256 = expected_sha256.map(|value| value.to_ascii_lowercase());
        ensure!(
            expected_sha256
                .as_deref()
                .is_none_or(delivery::valid_sha256),
            "SHA-256 必须为 64 位十六进制字符串"
        );
        self.db().tx(|q| {
            ticket_project(q, ticket)?;
            let current = contract_revision(q, ticket)?;
            ensure!(current == expected_revision, "需求合同已被修改，请刷新后重试");
            if let Some(job) = job {
                ensure!(q.exists("SELECT 1 FROM jobs WHERE id=? AND ticket=?", &[&job, &ticket])?, "关联任务不属于该工单");
            }
            let id = if let Some(id) = requirement_id {
                ensure!(q.exists("SELECT 1 FROM delivery_requirements WHERE id=? AND ticket=?", &[&id, &ticket])?, "需求不属于该工单");
                id
            } else {
                ensure!(q.int("SELECT COUNT(*) FROM delivery_requirements WHERE ticket=?", &[&ticket])? < delivery::MAX_REQUIREMENTS as i64, "每个工单最多 256 项需求");
                q.insert("INSERT INTO delivery_requirements(ticket) VALUES(?)", &[&ticket])?
            };
            let revision = q.int("SELECT COALESCE(MAX(revision),0)+1 FROM delivery_requirement_revisions WHERE requirement=?", &[&id])?;
            let next = current.checked_add(1).context("需求合同版本已超出限制")?;
            q.run("INSERT INTO delivery_requirement_revisions(requirement,revision,contract_revision,text,path,expected_sha256,job,source,created) VALUES(?,?,?,?,?,?,?,'operator',?)", &[&id, &revision, &next, &text, &path, &expected_sha256, &job, &now()])?;
            q.run("INSERT INTO delivery_contracts(ticket,revision) VALUES(?,?) ON CONFLICT(ticket) DO UPDATE SET revision=excluded.revision", &[&ticket, &next])?;
            event(q, ticket, "requirement.revised", json!({"schema_version":1,"requirement_id":id,"revision":revision,"contract_revision":next,"source":"operator"}))?;
            committed_summary(q, ticket, next)
        })
    }

    /// Capture exactly the explicitly selected project-relative files. Objects
    /// are durably published before a transaction may reference them. A crash
    /// can leave an unreferenced object. Readers verify every referenced object
    /// and project missing/corrupt dependencies as unknown. Each file is checked
    /// for stability, without claiming one atomic whole-repository timepoint.
    pub fn capture_delivery(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        paths: Vec<String>,
    ) -> Result<DeliverySnapshot> {
        ensure!(
            !paths.is_empty() && paths.len() <= delivery::MAX_FILES,
            "请明确选择 1 至 128 个交付文件"
        );
        let mut unique = HashSet::new();
        let paths = paths
            .iter()
            .map(|path| {
                let normalized = delivery::normalize_path(path)?;
                ensure!(
                    unique.insert(normalized.to_ascii_lowercase()),
                    "交付路径重复"
                );
                Ok(normalized)
            })
            .collect::<Result<Vec<_>>>()?;
        let (project, requirements) = self.db().with(|q| {
            let project = ticket_project(q, ticket)?;
            ensure!(
                contract_revision(q, ticket)? == expected_contract_revision,
                "需求合同已被修改，请刷新后捕获"
            );
            Ok((project, delivery::requirements(q.conn(), ticket)?))
        })?;
        let root = Path::new(&project);
        let mut files = Vec::new();
        let mut total = 0;
        for path in &paths {
            let source = delivery::project_file(root, path)?;
            let bytes = delivery::read_stable_file(&source)?;
            total += bytes.len() as u64;
            ensure!(
                total <= delivery::MAX_TOTAL_BYTES,
                "交付文件合计超过 64 MiB"
            );
            let file = DeliveryFile {
                path: path.clone(),
                sha256: delivery::sha256(&bytes),
                bytes: bytes.len() as u64,
            };
            publish_object(self.data_dir(), &file, &bytes)?;
            files.push(file);
        }
        // Re-read the entire selected input set after publication. A change to
        // an earlier file while later files were copied rejects the capture.
        for file in &files {
            let source = delivery::project_file(root, &file.path)?;
            ensure!(
                delivery::sha256(&delivery::read_stable_file(&source)?) == file.sha256,
                "捕获期间文件发生变化，请重新捕获"
            );
            delivery::read_object(self.data_dir(), file)?;
        }
        let evidence = requirements
            .iter()
            .map(|requirement| {
                if requirement.behavior.is_some() {
                    delivery::DeliveryEvidence {
                        requirement_id: requirement.id,
                        requirement_revision: requirement.revision,
                        status: "unknown".into(),
                        reason: "刚捕获的快照尚未执行已采用的行为检查".into(),
                        source: "platform_behavior".into(),
                    }
                } else {
                    delivery::evaluate_file_criterion(requirement, &files, &HashSet::new())
                }
            })
            .collect();
        let mut frozen = DeliverySnapshot {
            schema_version: if requirements.iter().any(|item| item.behavior.is_some()) {
                2
            } else {
                1
            },
            id: 0,
            ticket,
            contract_revision: expected_contract_revision,
            created_at: now(),
            files,
            requirements,
            evidence,
        };
        for (requirement, evidence) in frozen.requirements.iter_mut().zip(&frozen.evidence) {
            requirement.status = evidence.status.clone();
            requirement.evidence = Some(evidence.clone());
        }
        let digest = self.db().tx(|q| {
            ensure!(ticket_project(q, ticket)? == project, "工单项目目录已改变");
            ensure!(contract_revision(q, ticket)? == expected_contract_revision, "捕获期间需求合同已被修改，请重新捕获");
            let id = q.insert("INSERT INTO delivery_snapshots(ticket,contract_revision,created,payload,sha256) VALUES(?,?,?,'','')", &[&ticket, &expected_contract_revision, &frozen.created_at])?;
            frozen.id = id;
            let payload = serde_json::to_string(&frozen)?;
            ensure!(payload.len() <= delivery::MAX_MANIFEST_BYTES, "交付清单超过 2 MiB");
            q.run("UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?", &[&payload, &delivery::sha256(payload.as_bytes()), &id])?;
            event(q, ticket, "delivery.captured", json!({"schema_version":1,"snapshot_id":id,"contract_revision":expected_contract_revision,"manifest_sha256":delivery::sha256(payload.as_bytes()),"source":"platform_sha256"}))?;
            Ok(delivery::sha256(payload.as_bytes()))
        })?;
        if frozen.schema_version == 2 {
            delivery::read_snapshot_matching(self.data_dir(), ticket, frozen.id, &digest)
        } else {
            Ok(frozen)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineOptions;

    #[tokio::test]
    async fn delivery_commit_cas_rejects_changes_after_object_verification() {
        for change in [
            "contract",
            "policy_aba",
            "latest_snapshot",
            "manifest_digest",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let project = directory.path().join("project");
            fs::create_dir(&project).unwrap();
            fs::write(project.join("answer.txt"), b"accepted").unwrap();
            let engine = Engine::open(
                directory.path().join("ledger"),
                vec![],
                EngineOptions::default(),
            )
            .unwrap();
            engine.db().with(|q| {
                q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'fixture',?,0,'codex',1)", &[&project.to_string_lossy().as_ref()])?;
                Ok(())
            }).unwrap();
            engine
                .save_requirement(
                    1,
                    0,
                    None,
                    "Declared bytes".into(),
                    Some("answer.txt".into()),
                    Some(delivery::sha256(b"accepted")),
                    None,
                )
                .unwrap();
            let snapshot = engine
                .capture_delivery(1, 1, vec!["answer.txt".into()])
                .unwrap();
            engine.set_delivery_gate(1, 0, true).unwrap();
            let result = engine.prepare_delivery_inner(1, 1, snapshot.id, 1, || {
                match change {
                    "contract" => {
                        engine.save_requirement(
                            1,
                            1,
                            None,
                            "New required scope".into(),
                            None,
                            None,
                            None,
                        )?;
                    }
                    "policy_aba" => {
                        engine.set_delivery_gate(1, 1, false)?;
                        engine.set_delivery_gate(1, 2, true)?;
                    }
                    "latest_snapshot" => {
                        engine.capture_delivery(1, 1, vec!["answer.txt".into()])?;
                    }
                    "manifest_digest" => {
                        engine.db().with(|q| {
                            q.run(
                                "UPDATE delivery_snapshots SET sha256=? WHERE id=?",
                                &[&delivery::sha256(b"changed"), &snapshot.id],
                            )
                            .map(|_| ())
                        })?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            });
            assert!(result.is_err(), "{change}");
            assert_eq!(
                engine
                    .db()
                    .with(|q| q.int("SELECT COUNT(*) FROM delivery_assessments", &[]))
                    .unwrap(),
                0,
                "{change}"
            );
            assert_eq!(
                engine
                    .db()
                    .with(|q| q.int(
                        "SELECT COUNT(*) FROM events WHERE kind='delivery.assessed'",
                        &[]
                    ))
                    .unwrap(),
                0,
                "{change}"
            );
            engine.close().await;
        }
    }
}
