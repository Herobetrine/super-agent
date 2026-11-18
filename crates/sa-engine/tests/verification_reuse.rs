//! Verification and workspace efficiency (architecture §10): environment-
//! bound check reuse, delivery evidence without project checks, and evidence
//! for an unchanged tree when the delivery gate needs it.
mod common;
use common::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn init(path: &Path, files: &[(&str, &str)]) {
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    for (name, body) in files {
        std::fs::write(path.join(name), body).unwrap();
    }
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
}

fn report() -> String {
    wire("SA_REPORT", json!({"status": "done", "summary": "fixture"}))
}

fn checks_for(fx: &Fx, job: i64) -> Value {
    payloads(fx, &format!("SELECT payload FROM job_checks WHERE job={job}"))[0].clone()
}

/// A check program outside the project: its identity is part of the key.
fn tool(dir: &Path, marker: &Path, version: &str) -> Vec<String> {
    let marker = marker.to_string_lossy();
    if cfg!(windows) {
        let path = dir.join("sa-tool.cmd");
        let body = format!("@echo off\r\nrem {version}\r\necho run>>\"{marker}\"\r\nexit /b 0\r\n");
        std::fs::write(&path, body).unwrap();
        vec![path.to_string_lossy().into_owned()]
    } else {
        let path = dir.join("sa-tool");
        std::fs::write(&path, format!("#!/bin/sh\n# {version}\necho run >> '{marker}'\n")).unwrap();
        Command::new("chmod").arg("+x").arg(&path).status().unwrap();
        vec![path.to_string_lossy().into_owned()]
    }
}

fn runs(marker: &Path) -> usize {
    std::fs::read_to_string(marker).map(|s| s.lines().count()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_check_program_invalidates_a_cached_pass_for_the_same_tree() {
    let fx = Fx::new(
        script(|c| async move {
            std::fs::write(c.req.cwd.join("base.txt"), "passed\n").unwrap();
            Ok(report())
        }),
        t1_catalog(),
    );
    init(&fx.project, &[]);
    let bin = tempfile::tempdir().unwrap();
    let marker = fx.dir.path().join("tool-runs.txt");
    let argv = tool(bin.path(), &marker, "v1");
    fx.engine.set_project_checks(&fx.project.to_string_lossy(), vec![argv]).unwrap();
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    assert_eq!(runs(&marker), 1);
    let key = checks_for(&fx, root)["verification_key"].as_str().unwrap().to_string();
    // Same tree again, same program: the recorded pass is reused.
    std::fs::write(fx.project.join("base.txt"), "external edit\n").unwrap();
    fx.engine.follow(ticket, "restore", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(checks_for(&fx, root)["cached"], true);
    assert_eq!(runs(&marker), 1);
    // Same tree, different program: the pass no longer applies.
    tool(bin.path(), &marker, "v2 with a different size");
    std::fs::write(fx.project.join("base.txt"), "external edit\n").unwrap();
    fx.engine.follow(ticket, "restore again", false).unwrap();
    fx.finish(ticket, "done").await;
    let checks = checks_for(&fx, root);
    assert_eq!(checks["cached"], false, "{checks}");
    assert_ne!(checks["verification_key"].as_str().unwrap(), key);
    assert_eq!(runs(&marker), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_criteria_are_checked_on_completion_without_project_checks() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                return Ok(send(json!({"id": "writer", "write": true})));
            }
            if c.depth() > 0 {
                std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
            }
            Ok(report())
        }),
        t1_catalog(),
    );
    init(&fx.project, &[]);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine
        .save_requirement(ticket, 0, None, "Result file".into(), Some("result.txt".into()), Some(sha("42\n")), None)
        .unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let job = fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL")[0]["id"].as_i64().unwrap();
    let checks = checks_for(&fx, job);
    assert_eq!(checks["runs"], json!([]));
    assert_eq!(checks["delivery"]["status"], "ready", "{checks}");
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='check.started'"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enabled_gate_gets_evidence_for_an_unchanged_tree_once() {
    let fx = Fx::new(script(|_| async move { Ok(report()) }), t1_catalog());
    init(&fx.project, &[("result.txt", "42\n")]);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine
        .save_requirement(ticket, 0, None, "Result file".into(), Some("result.txt".into()), Some(sha("42\n")), None)
        .unwrap();
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    // No file changed in the turn, but the required item had no evidence.
    assert_eq!(fx.int("SELECT count(*) FROM delivery_assessments"), 1);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let checks = checks_for(&fx, root);
    assert_eq!(checks["unchanged"], true);
    assert_eq!(checks["delivery"]["status"], "ready", "{checks}");
    // Evidence is current for the same inputs: nothing is redone.
    fx.engine.follow(ticket, "look again", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM delivery_assessments"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM delivery_snapshots"), 1);
    fx.engine.close().await;
}
