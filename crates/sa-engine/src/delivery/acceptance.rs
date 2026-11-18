//! A human decision on an exact, already assessed scope.
use super::{behavior, event};
use crate::{
    Engine,
    db::{Q, now},
};
use anyhow::{Context, Result, ensure};
use sa_store::delivery::{self, DeliveryAssessment, DeliveryFile, DeliverySummary};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};
#[cfg(test)]
mod tests;

impl Engine {
    pub fn accept_delivery(
        &self,
        ticket: i64,
        expected_assessment_id: i64,
        expected_status: &str,
    ) -> Result<DeliverySummary> {
        self.accept_delivery_inner(ticket, expected_assessment_id, expected_status, || Ok(()))
    }

    fn accept_delivery_inner(
        &self,
        ticket: i64,
        assessment_id: i64,
        status: &str,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<DeliverySummary> {
        ensure!(
            ticket > 0 && assessment_id > 0 && matches!(status, "ready" | "partial"),
            "只能接受已核验的完整或部分交付"
        );
        let basis = self
            .db()
            .with(|q| version(q, ticket, assessment_id, status))?;
        let objects = ObjectWindow::open(self.data_dir(), &basis.manifest)?;
        // This read uses its own transaction. It verifies frozen object bytes
        // and recomputes the unmet scope without holding the journal writer.
        let mut summary = self.delivery_summary(ticket)?;
        let visible = summary
            .gate
            .acceptable_assessment()
            .context("核验状态或未满足项已改变，请重新核验并确认")?;
        ensure!(
            visible.id == assessment_id
                && visible.status == status
                && delivery::acceptance_digest(visible, &basis.behavior)? == basis.digest,
            "待接受的交付核验已改变，请刷新后确认"
        );
        before_commit()?;
        let accepted = self.db().tx(|q| {
            let latest = version(q,ticket,assessment_id,status)?;
            ensure!(latest.digest == basis.digest, "接受期间交付核验依据已改变，请刷新后确认");
            objects.confirm()?;
            let created = now();
            let inserted = q.run("INSERT INTO delivery_acceptances(ticket,assessment_id,assessment_status,assessment_sha256,created) VALUES(?,?,?,?,?) ON CONFLICT(assessment_id) DO NOTHING",
                &[&ticket,&assessment_id,&status,&basis.digest,&created])?;
            let accepted = delivery::gate_metadata(q.conn(), ticket)?.latest_acceptance
                .filter(|record| record.assessment_id == assessment_id && record.assessment_sha256 == basis.digest && record.status == status)
                .context("已有人工接受记录与当前核验不符，请重新核验并确认")?;
            if inserted == 1 {
                event(q,ticket,"delivery.accepted",json!({"source":"operator","acceptance_id":accepted.id,"assessment_id":assessment_id,
                    "snapshot_id":accepted.snapshot_id,"contract_revision":accepted.contract_revision,"policy_revision":accepted.policy_revision,
                    "assessment_sha256":basis.digest,"assessment_status":status}))?;
            }
            Ok(accepted)
        })?;
        summary.gate.latest_acceptance = Some(accepted.clone());
        summary.gate.acceptance = Some(accepted);
        Ok(summary)
    }
}

struct Version {
    digest: String,
    behavior: String,
    manifest: String,
}
fn version(q: &Q, ticket: i64, assessment_id: i64, status: &str) -> Result<Version> {
    let gate = delivery::gate_metadata(q.conn(), ticket)?;
    let current: DeliveryAssessment = gate.latest_assessment.context("尚无交付核验")?;
    ensure!(
        gate.enabled
            && current.ticket == ticket
            && current.id == assessment_id
            && current.status == status
            && current.policy_revision == gate.policy_revision,
        "最新交付核验、状态或策略已改变，请刷新后确认"
    );
    let (digest, manifest) = behavior::current_manifest_bytes(
        q,
        ticket,
        current.contract_revision,
        current.snapshot_id,
    )?;
    ensure!(
        current.manifest_sha256 == digest && delivery::assessment_matches(q.conn(), &current)?,
        "交付清单或行为检查依据已改变，请重新核验并确认"
    );
    let behavior = delivery::behavior_basis(q.conn(), ticket, current.snapshot_id)?;
    Ok(Version {
        digest: delivery::acceptance_digest(&current, &behavior)?,
        behavior,
        manifest,
    })
}

#[derive(Deserialize)]
struct Files {
    files: Vec<DeliveryFile>,
}

/// Freeze existing Windows object files and directory names across the read
/// and commit. Missing objects may be part of a disclosed partial scope.
/// On Unix also compare device, inode and ctime at commit for replacement or
/// in-place writes. Only metadata checks run inside the writer transaction.
struct ObjectWindow {
    entries: Vec<(PathBuf, Option<(fs::File, Stamp)>)>,
    #[cfg(windows)]
    _directories: Vec<fs::File>,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    bytes: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Stamp {
    fn read(meta: fs::Metadata) -> Result<Self> {
        ensure!(meta.is_file(), "冻结对象不再是普通文件");
        Ok(Self {
            bytes: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
        })
    }
}

impl ObjectWindow {
    fn open(data: &Path, manifest: &str) -> Result<Self> {
        let files: Files = serde_json::from_str(manifest)?;
        ensure!(
            files.files.len() <= delivery::MAX_FILES,
            "交付清单文件数超限"
        );
        let directory = data.join(delivery::OBJECT_DIRECTORY);
        delivery::reject_links(&directory)?;
        #[cfg(windows)]
        let dirs = {
            use std::os::windows::fs::OpenOptionsExt;
            [data, directory.as_path()]
                .iter()
                .map(|dir| {
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(3)
                        .custom_flags(0x0200_0000 | 0x0020_0000)
                        .open(dir)
                })
                .collect::<std::io::Result<Vec<_>>>()?
        };
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for file in files.files {
            ensure!(delivery::valid_sha256(&file.sha256), "交付对象摘要无效");
            let digest = file.sha256.to_ascii_lowercase();
            if !seen.insert(digest.clone()) {
                continue;
            }
            let path = directory.join(digest);
            match fs::symlink_metadata(&path) {
                Ok(meta) => ensure!(meta.is_file(), "交付对象不是普通文件"),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    entries.push((path, None));
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1).custom_flags(0x0020_0000);
            }
            let entry = match options.open(&path) {
                Ok(file) => {
                    delivery::reject_links(&path)?;
                    let stamp = Stamp::read(file.metadata()?)?;
                    Some((file, stamp))
                }
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            entries.push((path, entry));
        }
        Ok(Self {
            entries,
            #[cfg(windows)]
            _directories: dirs,
        })
    }

    fn confirm(&self) -> Result<()> {
        for (path, entry) in &self.entries {
            match entry {
                Some((_file, stamp)) => ensure!(
                    Stamp::read(fs::symlink_metadata(path)?)? == *stamp,
                    "接受期间冻结对象已改变"
                ),
                None => ensure!(
                    matches!(fs::symlink_metadata(path), Err(e) if e.kind() == ErrorKind::NotFound),
                    "接受期间缺失对象状态已改变"
                ),
            }
        }
        Ok(())
    }
}
