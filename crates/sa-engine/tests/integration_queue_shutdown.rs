//! Queue cancellation owns real validation descendants in disposable Git repos.
//! Fake model replies only create the isolated change. Git and sleep are real.
mod common;

use common::*;
use serde_json::json;
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn ps_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn sh_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

struct SleepProbe {
    started: std::path::PathBuf,
    survived: std::path::PathBuf,
    skip: std::path::PathBuf,
    command: Vec<String>,
}

impl SleepProbe {
    fn new(home: &Path) -> Self {
        let started = home.join("validation-child.pid");
        let survived = home.join("validation-child-survived");
        let skip = home.join("skip-sleep-on-revalidation");
        let child = home.join(if cfg!(windows) {
            "sleep-child.ps1"
        } else {
            "sleep-child.sh"
        });
        let command = if cfg!(windows) {
            std::fs::write(&child, format!(
                "Set-Content -LiteralPath {} -Value $PID\r\nStart-Sleep -Seconds 4\r\nSet-Content -LiteralPath {} -Value survived\r\n",
                ps_path(&started), ps_path(&survived),
            )).unwrap();
            vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!(
                    "if (($PWD.Path -like '*integrations*') -and !(Test-Path -LiteralPath {})) {{ $worker = Start-Process -FilePath powershell.exe -WindowStyle Hidden -ArgumentList @('-NoProfile','-NonInteractive','-File','\"{}\"') -PassThru; Wait-Process -Id $worker.Id }}; exit 0",
                    ps_path(&skip),
                    child.to_string_lossy().replace('\'', "''"),
                ),
            ]
        } else {
            std::fs::write(
                &child,
                format!(
                    "printf '%s' \"$$\" > {}\nsleep 4\nprintf survived > {}\n",
                    sh_path(&started),
                    sh_path(&survived),
                ),
            )
            .unwrap();
            vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "case \"$PWD\" in */integrations/*) if [ ! -f {} ]; then sh {} & wait; fi;; esac; exit 0",
                    sh_path(&skip),
                    sh_path(&child),
                ),
            ]
        };
        Self {
            started,
            survived,
            skip,
            command,
        }
    }

    fn pid(&self) -> Option<u32> {
        std::fs::read_to_string(&self.started)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn alive(&self) -> bool {
        let pid = self
            .pid()
            .expect("the descendant must report its PID first");
        let status = if cfg!(windows) {
            Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &format!(
                    "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}",
                )])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        } else {
            Command::new("/bin/kill").args(["-0", &pid.to_string()])
                .stdout(Stdio::null()).stderr(Stdio::null()).status()
        }.unwrap();
        status.success()
    }
}

async fn setup(fx: &Fx, probe: &SleepProbe) -> (i64, String) {
    git(&fx.project, &["init"]);
    git(&fx.project, &["config", "core.autocrlf", "false"]);
    std::fs::write(fx.project.join("base.txt"), "base\n").unwrap();
    git(&fx.project, &["add", "."]);
    git(&fx.project, &["commit", "-m", "base"]);
    let baseline = git(&fx.project, &["rev-parse", "HEAD"]);
    fx.engine
        .set_project_checks(&fx.project.to_string_lossy(), vec![probe.command.clone()])
        .unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    // Enable preparation only after the model work is terminal. This pins
    // cancellation coverage to the formerly missed all-jobs-done case.
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.until(|| probe.pid().is_some()).await;
    assert!(
        probe.alive(),
        "validation descendant was not observed running"
    );
    assert_eq!(
        root_state(&fx.engine, ticket),
        "done",
        "cancel must also cover model jobs that already ended"
    );
    let rows = fx.engine.integration_queue(ticket).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "validating");
    (ticket, baseline)
}

fn writer() -> Script {
    script(|context| async move {
        if context.depth() == 0 && context.turn() == 1 {
            return Ok(send(json!({"id":"writer", "body":"write", "write":true})));
        }
        if context.depth() > 0 {
            std::fs::write(context.req.cwd.join("result.txt"), "result\n").unwrap();
        }
        Ok("done".into())
    })
}

fn unchanged_source(fx: &Fx, baseline: &str) {
    assert_eq!(git(&fx.project, &["rev-parse", "HEAD"]), baseline);
    assert_eq!(
        git(
            &fx.project,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(fx.project.join("base.txt")).unwrap(),
        "base\n"
    );
    assert!(!fx.project.join("result.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_kills_candidate_check_descendants_and_restart_revalidates() {
    let mut fx = Fx::new(writer(), t1_catalog());
    let probe = SleepProbe::new(fx.dir.path());
    let (ticket, baseline) = setup(&fx, &probe).await;
    let calls = fx.shared.count(|_| true);
    tokio::time::timeout(Duration::from_secs(3), fx.restart(quiet()))
        .await
        .expect("close did not terminate the candidate check promptly");
    assert!(!probe.alive(), "validation descendant survived close");
    unchanged_source(&fx, &baseline);
    assert_eq!(
        fx.engine.integration_queue(ticket).unwrap()[0].state,
        "queued"
    );

    // Resume the same persisted candidate preparation with a passing check.
    // A preexisting marker bypasses only the sleep, without changing argv.
    std::fs::write(&probe.skip, "resume").unwrap();
    fx.engine.start().unwrap();
    fx.until(|| {
        fx.engine
            .integration_queue(ticket)
            .unwrap()
            .iter()
            .any(|item| item.state == "awaiting_confirmation")
    })
    .await;
    assert_eq!(
        fx.shared.count(|_| true),
        calls,
        "revalidation must not invoke the model again"
    );
    unchanged_source(&fx, &baseline);
    fx.engine.close().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !probe.survived.exists(),
        "the cancelled descendant reached its delayed write"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_after_models_finish_kills_candidate_check_descendants() {
    let fx = Fx::new(writer(), t1_catalog());
    let probe = SleepProbe::new(fx.dir.path());
    let (ticket, baseline) = setup(&fx, &probe).await;
    let calls = fx.shared.count(|_| true);
    fx.engine.cancel(ticket).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while probe.alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ticket cancel did not terminate the candidate descendant promptly");
    // Drain journal persistence before checking that it can never publish.
    fx.engine.quiesce().await;
    unchanged_source(&fx, &baseline);
    assert!(
        fx.engine
            .integration_queue(ticket)
            .unwrap()
            .iter()
            .all(|item| !matches!(
                item.state.as_str(),
                "awaiting_confirmation" | "publishing" | "merged"
            ))
    );
    assert_eq!(fx.shared.count(|_| true), calls);
    fx.engine.close().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !probe.survived.exists(),
        "the cancelled descendant reached its delayed write"
    );
}
