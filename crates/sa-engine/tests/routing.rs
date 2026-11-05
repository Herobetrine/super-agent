mod common;

use common::*;
use futures::future::BoxFuture;
use sa_engine::services::{FsSnapshotter, Snapshotter};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn catalog() -> Value {
    let mut value = t1_catalog();
    value["providers"][0]["models"] = json!([
        {"id":"default", "reasoningEfforts":["low","high"], "defaultReasoningEffort":"low"},
        {"id":"review", "reasoningEfforts":["low","high"]},
        {"id":"plain"}
    ]);
    value
}

fn route(model: &str, effort: &str) -> Value {
    json!({"provider":"test-local", "model":model, "mode":"cli", "reasoning_effort":effort})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collaborator_effort_survives_replay_and_reopen_while_lead_stays_fixed() {
    for explicit_model in [true, false] {
        let mut fx = Fx::new(
            script(move |c| async move {
                if c.depth() == 0 {
                    assert_eq!(c.req.route.model, "default");
                    assert_eq!(c.req.route.reasoning_effort.as_deref(), Some("low"));
                    if c.turn() == 1 {
                        let mut fields = json!({"reasoning_effort":"high"});
                        if explicit_model {
                            fields["model"] = json!("review");
                        }
                        let command = send(fields);
                        return Ok(format!("{command}\n{command}"));
                    }
                } else {
                    assert_eq!(c.req.route.model, "review");
                    assert_eq!(c.req.route.reasoning_effort.as_deref(), Some("high"));
                    assert_eq!(c.packet["allow_lead_routing"], json!(false));
                }
                Ok("verified".into())
            }),
            catalog(),
        );
        let mut params = fx.params();
        params.selections =
            Some(json!({"codex":route("default", "low"), "deepseek":route("review", "low")}));
        let ticket = fx.begin(params).await;
        fx.finish(ticket, "done").await;
        assert_eq!(
            fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"),
            1
        );
        let stored = fx.all("SELECT route_override FROM jobs WHERE parent IS NOT NULL");
        let selected: Value =
            serde_json::from_str(stored[0]["route_override"].as_str().unwrap()).unwrap();
        assert_eq!(selected, route("review", "high"));
        fx.restart(quiet()).await;
        fx.engine.follow(ticket, "继续核对", false).unwrap();
        fx.engine.start().unwrap();
        fx.finish(ticket, "done").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_collaborator_selection_is_used_without_overrides() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                assert_eq!(c.packet["allow_lead_routing"], json!(false));
                return Ok(send(json!({})));
            }
            if c.depth() > 0 {
                assert_eq!(c.req.route.model, "review");
                assert_eq!(c.req.route.reasoning_effort.as_deref(), Some("high"));
            }
            Ok("verified".into())
        }),
        catalog(),
    );
    let mut params = fx.params();
    params.allow_lead_routing = Some(false);
    params.selections = Some(json!({"deepseek":route("review", "high")}));
    let ticket = fx.begin(params).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_effort_and_changed_replay_are_rejected() {
    for fields in [
        json!({"model":"plain", "reasoning_effort":"high"}),
        json!({"model":"review", "reasoning_effort":"max"}),
        json!({"model":"review", "reasoning_effort":null}),
    ] {
        let fx = Fx::new(
            script(move |_| {
                let command = send(fields.clone());
                async move { Ok(command) }
            }),
            catalog(),
        );
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "failed").await;
        assert_eq!(fx.int("SELECT count(*) FROM jobs"), 1);
        assert!(
            fx.engine.root(ticket).unwrap()["result"]
                .as_str()
                .unwrap()
                .contains("思考强度")
        );
    }
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                return Ok([
                    send(json!({"model":"review", "reasoning_effort":"low"})),
                    send(json!({"model":"review", "reasoning_effort":"high"})),
                ]
                .join("\n"));
            }
            Ok("verified".into())
        }),
        catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "failed").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"),
        1
    );
    assert!(
        fx.engine.root(ticket).unwrap()["result"]
            .as_str()
            .unwrap()
            .contains("同一消息 ID")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collaborator_cannot_select_a_grandchild_effort() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() == 1 && c.depth() == 0 {
                return Ok(send(json!({})));
            }
            if c.depth() == 1 {
                return Ok(send(json!({"to":"claude", "reasoning_effort":"high"})));
            }
            Ok("reviewed failure".into())
        }),
        catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE depth=2"), 0);
    assert!(
        fx.all("SELECT result FROM jobs WHERE depth=1")[0]["result"]
            .as_str()
            .unwrap()
            .contains("当前获授权的主控")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_policy_changes_apply_to_new_dispatches_and_persist() {
    for initial in [true, false] {
        let entered = Gate::new();
        let release = Gate::new();
        let (started, wait) = (entered.clone(), release.clone());
        let mut fx = Fx::new(
            script(move |c| {
                let (started, wait) = (started.clone(), wait.clone());
                async move {
                    if c.depth() == 0 && c.turn() == 1 {
                        assert_eq!(c.packet["allow_lead_routing"], json!(initial));
                        started.open();
                        wait.wait().await;
                        return Ok(send(json!({"model":"review", "reasoning_effort":"high"})));
                    }
                    Ok("verified".into())
                }
            }),
            catalog(),
        );
        let mut params = fx.params();
        params.allow_lead_routing = Some(initial);
        let ticket = fx.begin(params).await;
        tokio::time::timeout(Duration::from_secs(5), entered.wait())
            .await
            .unwrap();
        fx.engine.set_lead_routing(ticket, !initial).unwrap();
        fx.engine.set_lead_routing(ticket, !initial).unwrap();
        release.open();
        fx.finish(ticket, if initial { "failed" } else { "done" })
            .await;
        assert_eq!(
            fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"),
            i64::from(!initial)
        );
        assert_eq!(
            fx.int("SELECT count(*) FROM events WHERE kind='lead_routing.changed'"),
            1
        );
        fx.restart(quiet()).await;
        assert_eq!(
            fx.int("SELECT allow_lead_routing FROM tickets"),
            i64::from(!initial)
        );
        // A host can roll policy and route settings back as one transaction.
        let result: anyhow::Result<()> = fx.engine.db().tx(|q| {
            q.set_lead_routing(ticket, initial)?;
            anyhow::bail!("synthetic route-save failure");
        });
        assert!(result.is_err());
        assert_eq!(
            fx.int("SELECT allow_lead_routing FROM tickets"),
            i64::from(!initial)
        );
        assert_eq!(
            fx.int("SELECT count(*) FROM events WHERE kind='lead_routing.changed'"),
            1
        );
    }
}

struct PausedSnapshot {
    entered: Gate,
    release: Gate,
}

impl Snapshotter for PausedSnapshot {
    fn snapshot(
        &self,
        project: PathBuf,
        names: Vec<String>,
        directory: PathBuf,
    ) -> BoxFuture<'static, anyhow::Result<Value>> {
        let (entered, release) = (self.entered.clone(), self.release.clone());
        Box::pin(async move {
            entered.open();
            release.wait().await;
            FsSnapshotter.snapshot(project, names, directory).await
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_policy_during_attachment_snapshot_prevents_commit() {
    let entered = Gate::new();
    let release = Gate::new();
    let mut opts = quiet();
    opts.snapshotter = Arc::new(PausedSnapshot {
        entered: entered.clone(),
        release: release.clone(),
    });
    let fx = Fx::with(
        script(|_| async {
            Ok(send(
                json!({"model":"review", "reasoning_effort":"high", "files":["context.txt"]}),
            ))
        }),
        catalog(),
        opts,
    );
    std::fs::write(fx.project.join("context.txt"), b"immutable context").unwrap();
    let ticket = fx.begin(fx.params()).await;
    tokio::time::timeout(Duration::from_secs(5), entered.wait())
        .await
        .unwrap();
    fx.engine.set_lead_routing(ticket, false).unwrap();
    release.open();
    fx.finish(ticket, "failed").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM job_files"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='send'"), 0);
}
