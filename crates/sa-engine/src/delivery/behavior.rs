//! Operator-adopted argv, executed once against a materialized frozen manifest.
use super::{committed_summary, contract_revision, event, ticket_project};
use crate::{
    Engine,
    db::{Q, now},
    integration_queue, workspaces,
};
use anyhow::{Context, Result, ensure};
use sa_store::delivery::{
    self, BehaviorCheck, BehaviorReceipt, DeliveryFile, DeliverySummary, RequirementView,
};
use serde_json::json;
use std::{collections::HashSet, fs, io::Write, path::Path, time::Duration};

mod host;
#[cfg(test)]
mod tests;

const TIMEOUT: Duration = Duration::from_secs(600);

fn pinned_files(project: &Path, paths: Vec<String>) -> Result<Vec<DeliveryFile>> {
    ensure!(paths.len() <= delivery::MAX_FILES, "验证器文件过多");
    let mut names = HashSet::new();
    let mut total = 0;
    let mut files = vec![];
    for path in paths {
        let path = delivery::normalize_path(&path)?;
        ensure!(names.insert(path.to_ascii_lowercase()), "验证器路径重复");
        let source = delivery::project_file(project, &path)?;
        let bytes = delivery::read_stable_file(&source)?;
        total += bytes.len() as u64;
        ensure!(total <= delivery::MAX_TOTAL_BYTES, "验证器文件总量过大");
        files.push(DeliveryFile {
            path,
            sha256: delivery::sha256(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    verify_files(project, &files)?;
    Ok(files)
}

fn requirement(q: &Q, ticket: i64, id: i64) -> Result<RequirementView> {
    delivery::requirements(q.conn(), ticket)?
        .into_iter()
        .find(|r| r.id == id)
        .context("需求不属于该工单")
}

impl Engine {
    pub fn adopt_delivery_check(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        requirement_id: i64,
        argv: Vec<String>,
    ) -> Result<DeliverySummary> {
        self.adopt_delivery_check_with_files(
            ticket,
            expected_contract_revision,
            requirement_id,
            argv,
            vec![],
        )
    }

    pub fn adopt_delivery_check_with_files(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        requirement_id: i64,
        argv: Vec<String>,
        verifier_paths: Vec<String>,
    ) -> Result<DeliverySummary> {
        let _power = self.track_work();
        workspaces::validate_command(&argv)?;
        let serialized = serde_json::to_string(&argv)?;
        ensure!(serialized.len() <= 16 * 1024, "行为检查命令超过 16 KiB");
        let project = self.db().with(|q| ticket_project(q, ticket))?;
        host::preflight(&argv[0], Path::new(&project), self.data_dir())?;
        let files = pinned_files(Path::new(&project), verifier_paths)?;
        let verifiers = serde_json::to_string(&files)?;
        self.db().tx(|q| {
            ensure!(ticket_project(q, ticket)? == project, "工单项目已改变");
            ensure!(contract_revision(q, ticket)? == expected_contract_revision, "需求合同已被修改，请刷新后重试");
            ensure!(integration_queue::commands(q, ticket)?.contains(&argv), "仅能采用当前项目已配置的完整检查命令");
            let item = requirement(q, ticket, requirement_id)?;
            ensure!(!item.withdrawn, "已撤销的需求不能采用行为检查");
            let revision = expected_contract_revision.checked_add(1).context("需求合同版本已超出限制")?;
            let binding = q.insert("INSERT INTO delivery_behavior_bindings(requirement,requirement_revision,contract_revision,argv,verifier_files,created) VALUES(?,?,?,?,?,?)",
                &[&requirement_id, &item.revision, &revision, &serialized, &verifiers, &now()])?;
            q.run("UPDATE delivery_contracts SET revision=? WHERE ticket=?", &[&revision, &ticket])?;
            event(q, ticket, "delivery.check_adopted", json!({"binding_id":binding,"requirement_id":requirement_id,
                "requirement_revision":item.revision,"contract_revision":revision,"source":"operator"}))?;
            committed_summary(q, ticket, revision)
        })
    }

    pub fn run_delivery_check(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        snapshot_id: i64,
        requirement_id: i64,
    ) -> Result<BehaviorReceipt> {
        self.run_delivery_check_with_cancel(
            ticket,
            expected_contract_revision,
            snapshot_id,
            requirement_id,
            &|| false,
        )
    }

    pub fn run_delivery_check_with_cancel(
        &self,
        ticket: i64,
        expected_contract_revision: i64,
        snapshot_id: i64,
        requirement_id: i64,
        cancel: &dyn Fn() -> bool,
    ) -> Result<BehaviorReceipt> {
        self.run_check_inner(
            ticket,
            expected_contract_revision,
            snapshot_id,
            requirement_id,
            cancel,
            TIMEOUT,
        )
    }

    fn run_check_inner(
        &self,
        ticket: i64,
        contract: i64,
        snapshot_id: i64,
        requirement_id: i64,
        cancel: &dyn Fn() -> bool,
        timeout: Duration,
    ) -> Result<BehaviorReceipt> {
        let _power = self.track_work();
        // Reserve a new attempt before touching the filesystem or resolving
        // the command. A crash or slow new attempt immediately supersedes any pass.
        // Its operation receipt is written in the same transaction.
        use crate::operations::{self as ops, kind};
        let scope = self.operation_scope(ops::Owner::ticket(ticket));
        let (basis, attempt, mut environment, operation) = self.db().tx(|q| {
            let project = ticket_project(q, ticket)?;
            let manifest = current_manifest(q, ticket, contract, snapshot_id)?;
            let item = requirement(q, ticket, requirement_id)?;
            ensure!(!item.withdrawn, "需求已撤销");
            let check = item.behavior.as_ref().context("需求没有已采用的行为检查")?;
            ensure!(check.requirement_revision == item.revision, "需求文字已修订，请重新采用检查");
            workspaces::validate_command(&check.argv)?;
            let environment = json!({"runner":delivery::BEHAVIOR_RUNNER,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
                "argv_sha256":delivery::sha256(&serde_json::to_vec(&check.argv)?),"cwd":"fresh_manifest_directory",
                "environment_policy":delivery::BEHAVIOR_ENVIRONMENT_POLICY,"timeout_ms":timeout.as_millis() as u64});
            let id = q.insert("INSERT INTO delivery_behavior_attempts(ticket,requirement,requirement_revision,binding_id,snapshot_id,contract_revision,manifest_sha256,started,environment) VALUES(?,?,?,?,?,?,?,?,?)",
                &[&ticket,&requirement_id,&item.revision,&check.id,&snapshot_id,&contract,&manifest,&now(),&environment.to_string()])?;
            event(q, ticket, "delivery.check_started", json!({"attempt_id":id,"binding_id":check.id,"snapshot_id":snapshot_id,"contract_revision":contract}))?;
            let operation = scope.begin_in(q, kind::BEHAVIOR, json!({"attempt":id,"requirement":requirement_id,
                "snapshot":snapshot_id,"binding":check.id,"argv_sha256":environment["argv_sha256"]}))?;
            Ok((Basis {ticket, contract, snapshot_id, item, project, manifest}, id, environment, operation))
        })?;
        scope.point(kind::BEHAVIOR, "after_begin")?;
        let stop = scope.cancellable(operation);
        let cancelled = || self.is_stopping() || cancel() || stop.load(std::sync::atomic::Ordering::SeqCst);
        let executed = basis.execute(self.data_dir(), &mut environment, &cancelled, timeout);
        let failure = executed.as_ref().err().map(|error| ops::process_failure(error, ops::FAILED_BEFORE_EFFECT));
        let outcome = executed.unwrap_or_else(|error| Outcome::invalid(format!("{error:#}")));
        scope.effect(kind::BEHAVIOR)?;
        let cancelled_before_commit = cancelled();
        // The owned process tree is always reaped before `execute` returns Ok.
        let state = failure.unwrap_or(if cancelled_before_commit { ops::CANCELLED } else { ops::CONFIRMED });
        scope.decide(operation, state, json!({"attempt":attempt,"status":outcome.status}));
        let verdicts = scope.settle(true);
        let (receipt, stale) = self.db().tx(|q| {
            scope.record(q, &verdicts, true)?;
            let stale = basis.confirm(q, attempt).err().map(|error| error.to_string());
            let outcome = if let Some(reason) = &stale { outcome.invalidate(reason) }
                else if cancelled_before_commit || self.is_stopping() { outcome.invalidate("提交行为检查回执前已取消") } else { outcome };
            q.run("UPDATE delivery_behavior_attempts SET environment=? WHERE id=?", &[&environment.to_string(), &attempt])?;
            q.run("INSERT INTO delivery_behavior_results(attempt_id,status,exit_code,detail,finished) VALUES(?,?,?,?,?)",
                &[&attempt,&outcome.status,&outcome.exit,&bounded(outcome.detail, 16384),&now()])?;
            event(q, ticket, "delivery.check_finished", json!({"attempt_id":attempt,"binding_id":basis.check().id,
                "snapshot_id":snapshot_id,"status":outcome.status,"contract_revision":contract}))?;
            Ok((delivery::behavior_receipt(q.conn(), attempt)?, stale))
        })?;
        if let Some(reason) = stale {
            anyhow::bail!("{reason}");
        }
        Ok(receipt)
    }
}

pub(super) fn current_manifest(
    q: &Q,
    ticket: i64,
    contract: i64,
    snapshot_id: i64,
) -> Result<String> {
    Ok(current_manifest_bytes(q, ticket, contract, snapshot_id)?.0)
}

pub(super) fn current_manifest_bytes(
    q: &Q,
    ticket: i64,
    contract: i64,
    snapshot_id: i64,
) -> Result<(String, String)> {
    ensure!(
        contract_revision(q, ticket)? == contract,
        "需求合同已改变，请刷新后重试"
    );
    ensure!(
        snapshot_id > 0
            && q.int(
                "SELECT COALESCE(MAX(id),0) FROM delivery_snapshots WHERE ticket=?",
                &[&ticket]
            )? == snapshot_id,
        "交付快照已改变或不存在，请刷新后重试"
    );
    let (digest,payload): (String,String) = q.conn().query_row(
        "SELECT sha256,payload FROM delivery_snapshots WHERE id=? AND ticket=? AND contract_revision=? AND length(CAST(sha256 AS BLOB))=64 AND length(CAST(payload AS BLOB))<=?",
        (snapshot_id,ticket,contract,delivery::MAX_MANIFEST_BYTES as i64), |row| Ok((row.get(0)?,row.get(1)?)))
        .context("交付快照不属于当前合同或清单无效")?;
    let digest = digest.to_ascii_lowercase();
    ensure!(
        delivery::sha256(payload.as_bytes()) == digest,
        "交付清单摘要不匹配"
    );
    Ok((digest, payload))
}

struct Basis {
    ticket: i64,
    contract: i64,
    snapshot_id: i64,
    item: RequirementView,
    project: String,
    manifest: String,
}

impl Basis {
    fn check(&self) -> &BehaviorCheck {
        self.item.behavior.as_ref().expect("adopted check")
    }

    fn confirm(&self, q: &Q, attempt: i64) -> Result<()> {
        ensure!(
            ticket_project(q, self.ticket)? == self.project
                && current_manifest(q, self.ticket, self.contract, self.snapshot_id)?
                    == self.manifest,
            "检查期间快照清单或项目已改变"
        );
        let current = requirement(q, self.ticket, self.item.id)?;
        ensure!(
            current.revision == self.item.revision
                && !current.withdrawn
                && current
                    .behavior
                    .as_ref()
                    .is_some_and(|binding| binding.same_definition(self.check())),
            "检查期间采用的需求标准已改变"
        );
        ensure!(q.int("SELECT MAX(id) FROM delivery_behavior_attempts WHERE ticket=? AND snapshot_id=? AND requirement=?",
            &[&self.ticket, &self.snapshot_id, &self.item.id])? == attempt, "已有更新的行为检查，本次结果已过期");
        Ok(())
    }

    fn execute(
        &self,
        data: &Path,
        environment: &mut serde_json::Value,
        cancel: &dyn Fn() -> bool,
        timeout: Duration,
    ) -> Result<Outcome> {
        ensure!(!cancel(), "行为检查已取消");
        let snapshot =
            delivery::read_snapshot_matching(data, self.ticket, self.snapshot_id, &self.manifest)?;
        let frozen = snapshot
            .requirements
            .iter()
            .find(|item| item.id == self.item.id)
            .context("冻结清单缺少该需求")?;
        ensure!(
            snapshot.contract_revision == self.contract
                && frozen.revision == self.item.revision
                && frozen.text == self.item.text
                && frozen
                    .behavior
                    .as_ref()
                    .is_some_and(|binding| binding.same_definition(self.check())),
            "冻结清单与采用的需求标准不符"
        );
        for verifier in &self.check().verifier_files {
            ensure!(
                snapshot.files.contains(verifier),
                "冻结清单中验证器文件缺失或已改变；清单路径拼写和大小写须完全相同：{}",
                verifier.path
            );
        }
        ensure!(!snapshot.files.is_empty(), "行为检查需要显式捕获输入");
        // A worktree's .git file can redirect repository discovery to an
        // unrelated live repository even when its pointer bytes are frozen.
        for file in &snapshot.files {
            ensure!(
                !file
                    .path
                    .split('/')
                    .any(|part| part.eq_ignore_ascii_case(".git")),
                "行为检查不能物化 Git 元数据路径：{}",
                file.path
            );
        }
        let temp = host::temp_dir(Path::new(&self.project), data)?;
        let workspace = temp.path().join("inputs");
        fs::create_dir(&workspace)?;
        for file in &snapshot.files {
            ensure!(!cancel(), "行为检查已取消");
            let path = workspace.join(&file.path);
            fs::create_dir_all(path.parent().context("输入路径无效")?)?;
            let mut output = fs::File::create_new(&path)?;
            output.write_all(&delivery::read_object(data, file)?)?;
        }
        let host = host::Execution::prepare(
            temp.path(),
            Path::new(&self.project),
            data,
            &self.check().argv,
            environment,
            cancel,
        )?;
        #[cfg(windows)]
        let _input_handles = lock_inputs(temp.path(), &workspace, &snapshot.files)?;
        verify_files(&workspace, &snapshot.files)?;
        // Any failure after a successful spawn leaves the run's effect unknown.
        let result = workspaces::delivery_command(&workspace, &host.argv, &host.env, timeout, cancel)
            .map_err(|error| if error.is::<crate::operations::NotStarted>() || error.is::<crate::operations::ProcessUnconfirmed>() {
                error
            } else {
                error.context(crate::operations::ProcessUnconfirmed)
            })?;
        let tail = String::from_utf8_lossy(&result.output);
        let observed = Outcome {
            status: match (result.ok, result.exit) {
                (true, Some(0)) => "pass",
                (_, Some(_)) => "fail",
                (_, None) => "invalid",
            },
            exit: result.exit,
            detail: match result.exit {
                Some(exit) => format!("退出码 {exit}。{tail}"),
                None => format!("未取得退出码。{tail}"),
            },
        };
        let verification = (|| -> Result<()> {
            ensure!(!result.cancelled && !cancel(), "行为检查已取消");
            ensure!(!result.timed_out, "行为检查超时，已终止进程");
            verify_files(&workspace, &snapshot.files)?;
            for file in &snapshot.files {
                delivery::read_object(data, file)?;
            }
            host.verify(cancel)
        })();
        Ok(match verification {
            Ok(()) => observed,
            Err(error) => observed.invalidate(&format!("{error:#}")),
        })
    }
}

fn verify_files(root: &Path, files: &[DeliveryFile]) -> Result<()> {
    for file in files {
        let path = delivery::project_file(root, &file.path)
            .with_context(|| format!("无法核验输入 {}", file.path))?;
        let bytes = delivery::read_stable_file(&path)?;
        ensure!(
            bytes.len() as u64 == file.bytes && delivery::sha256(&bytes) == file.sha256,
            "输入文件已改变：{}",
            file.path
        );
    }
    Ok(())
}

/// Hold the original directory entries in place while the process tree is
/// running. On Windows an attempted write, delete or rename must fail before
/// it changes an input, even if the command would subsequently restore it.
#[cfg(windows)]
fn lock_inputs(temp: &Path, root: &Path, files: &[DeliveryFile]) -> Result<Vec<fs::File>> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut guards = Vec::new();
    let mut directories = HashSet::new();
    for file in files {
        let path = root.join(&file.path);
        for dir in path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(temp))
        {
            if directories.insert(dir.to_path_buf()) {
                delivery::reject_links(dir)?;
                // Read/write sharing allows unrelated generated files in this
                // directory. Denying delete sharing pins the directory name.
                guards.push(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(3)
                        .custom_flags(0x0200_0000 | 0x0020_0000)
                        .open(dir)?,
                );
            }
        }
        delivery::reject_links(&path)?;
        guards.push(
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .custom_flags(0x0020_0000)
                .open(path)?,
        );
    }
    Ok(guards)
}

struct Outcome {
    status: &'static str,
    exit: Option<i32>,
    detail: String,
}
impl Outcome {
    fn invalidate(self, reason: &str) -> Self {
        Self {
            status: "invalid",
            exit: self.exit,
            detail: format!("{reason}。本次观察状态 {}。{}", self.status, self.detail),
        }
    }

    fn invalid(detail: String) -> Self {
        Self {
            status: "invalid",
            exit: None,
            detail,
        }
    }
}

fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
