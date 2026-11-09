use sa_engine::{db::Db, scm};
use serde_json::{Value, json};

const URL: &str = "https://github.com/example/project/pull/42";

fn setup() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    db.tx(|q| {
        q.0.execute_batch(scm::SCHEMA)?;
        q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'test','.',1,'codex',1)", &[])?;
        for job in 1..=2 {
            q.run("INSERT INTO jobs(id,ticket,agent,body,writing,depth,state) VALUES(?,1,'codex','test',0,0,'cancelled')", &[&job])?;
        }
        scm::bind(q, 1, URL)?;
        Ok(())
    }).unwrap();
    (dir, db)
}

fn observation() -> Value {
    json!({
        "url":URL,"number":42,"title":"Review this change","state":"OPEN","isDraft":false,
        "headRefOid":"a".repeat(40),"baseRefName":"main","mergeable":"MERGEABLE",
        "mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
        "statusCheckRollup":[
            {"name":"build","conclusion":"SUCCESS","status":"COMPLETED","detailsUrl":"https://github.com/example/project/actions/runs/1"},
            {"context":"lint","state":"SUCCESS","targetUrl":"https://ci.example/1"}
        ],
        "latestReviews":[{"id":"review1","author":{"login":"reviewer"},"state":"APPROVED","body":"Looks good","submittedAt":"2026-11-09T01:00:00Z"}],
        "_sa_observed_at":1000.0
    })
}

#[test]
fn observations_are_durable_deduplicated_and_order_independent() {
    let (dir, db) = setup();
    let raw = observation();
    assert!(db.tx(|q| scm::apply(q, 1, &raw)).unwrap());
    let mut reordered = raw.clone();
    reordered["statusCheckRollup"]
        .as_array_mut()
        .unwrap()
        .reverse();
    reordered["_sa_observed_at"] = json!(1001.0);
    assert!(!db.tx(|q| scm::apply(q, 1, &reordered)).unwrap());
    db.close();
    let db = Db::open(dir.path()).unwrap();
    assert!(!db.tx(|q| scm::apply(q, 1, &reordered)).unwrap());
    reordered["headRefOid"] = json!("b".repeat(40));
    assert!(db.tx(|q| scm::apply(q, 1, &reordered)).unwrap());
    reordered["latestReviews"][0]["body"] = json!("Please fix the error handling");
    reordered["latestReviews"][0]["state"] = json!("CHANGES_REQUESTED");
    reordered["reviewDecision"] = json!("CHANGES_REQUESTED");
    assert!(db.tx(|q| scm::apply(q, 1, &reordered)).unwrap());
    db.with(|q| {
        assert_eq!(q.int("SELECT count(*) FROM scm_observations", &[])?, 3);
        assert_eq!(
            q.int("SELECT count(*) FROM events WHERE kind='scm.feedback'", &[])?,
            3
        );
        assert_eq!(scm::bindings(q, 1)?[0]["pending"], 3);
        Ok(())
    })
    .unwrap();
}

#[test]
fn external_facts_never_wake_jobs_or_approve_permissions() {
    let (_dir, db) = setup();
    for (i, state) in ["cancelled", "waiting", "running", "done", "failed"]
        .iter()
        .enumerate()
    {
        db.tx(|q| {
            q.run("UPDATE jobs SET state=? WHERE id=1", &[state])?;
            q.run("INSERT INTO approvals(ticket,job,turn,action,decision) VALUES(1,1,1,'fixture','pending')", &[])?;
            let before = q.one("SELECT * FROM jobs WHERE id=1", &[])?.unwrap();
            let mut raw = observation();
            raw["_sa_observed_at"] = json!(1000.0 + i as f64);
            raw["title"] = json!(format!("observation {i}"));
            assert!(scm::apply(q, 1, &raw)?);
            assert_eq!(q.one("SELECT * FROM jobs WHERE id=1", &[])?.unwrap(), before);
            assert_eq!(q.int("SELECT count(*) FROM approvals WHERE decision!='pending'", &[])?, 0);
            assert_eq!(q.int("SELECT count(*) FROM mailbox", &[])?, 0);
            Ok(())
        }).unwrap();
    }
    assert_eq!(db.with(|q| scm::context(q, 1)).unwrap().len(), 5);
}

#[test]
fn rebinding_stale_reads_and_acknowledgements_are_job_scoped() {
    let (_dir, db) = setup();
    db.tx(|q| {
        let raw = observation();
        scm::bind(q, 2, URL)?;
        scm::apply(q, 1, &raw)?;
        scm::apply(q, 2, &raw)?;
        let first = scm::context(q, 1)?[0]["id"].as_i64().unwrap();
        let other = scm::context(q, 2)?[0]["id"].as_i64().unwrap();
        scm::acknowledge(q, 1, &[first, other], 7)?;
        assert!(scm::context(q, 1)?.is_empty());
        assert_eq!(scm::context(q, 2)?.len(), 1);
        assert_eq!(
            q.int(
                "SELECT consumed_turn FROM scm_observations WHERE id=?",
                &[&first]
            )?,
            7
        );
        let mut stale = raw.clone();
        stale["_sa_observed_at"] = json!(999.0);
        stale["title"] = json!("stale");
        assert!(!scm::apply(q, 1, &stale)?);
        assert_eq!(
            q.int("SELECT count(*) FROM scm_observations WHERE job=1", &[])?,
            1
        );
        scm::bind(q, 1, "https://github.com/example/project/pull/43")?;
        assert!(scm::apply(q, 1, &raw).is_err());
        assert!(scm::context(q, 1)?.is_empty());
        assert_eq!(scm::bindings(q, 1)?[0]["latest"], Value::Null);
        assert!(scm::bind(q, 999, URL).is_err());
        assert!(
            scm::bind(
                q,
                1,
                "https://github.com/example/project/pull/43?token=secret"
            )
            .is_err()
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn summaries_preserve_unknown_conflicts_and_truncation_instead_of_claiming_success() {
    let mut raw = observation();
    raw["mergeable"] = json!("CONFLICTING");
    raw["mergeStateStatus"] = json!("DIRTY");
    raw["statusCheckRollup"] = json!([]);
    assert_eq!(scm::summarize(&raw).unwrap()["ci"], "unknown");
    raw["statusCheckRollup"] =
        json!([{"name":"build","conclusion":"CANCELLED","status":"COMPLETED"}]);
    assert_eq!(scm::summarize(&raw).unwrap()["ci"], "cancelled");
    raw["statusCheckRollup"] = json!([{"name":"build","conclusion":null,"status":"IN_PROGRESS"}]);
    assert_eq!(scm::summarize(&raw).unwrap()["ci"], "pending");
    raw["statusCheckRollup"] = json!([{"context":"external","state":"ERROR"}]);
    assert_eq!(scm::summarize(&raw).unwrap()["ci"], "failing");
    let mut checks: Vec<Value> = (0..110)
        .map(|i| json!({"name":format!("check{i}"),"conclusion":"SUCCESS"}))
        .collect();
    checks.push(json!({"name":"zzzz-truncated-failure","conclusion":"FAILURE"}));
    raw["statusCheckRollup"] = json!(checks);
    raw["latestReviews"][0]["body"] = json!("评审".repeat(1000));
    raw["title"] = json!("x\u{001b}y".repeat(1000));
    let summary = scm::summarize(&raw).unwrap();
    assert_eq!(summary["ci"], "failing");
    assert_eq!(summary["mergeable"], "CONFLICTING");
    assert_eq!(summary["partial"], true);
    assert_eq!(summary["checks"].as_array().unwrap().len(), 100);
    assert_eq!(
        summary["reviews"][0]["body"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        800
    );
    assert!(!summary["title"].as_str().unwrap().contains('\u{001b}'));
    assert_eq!(summary["source"], "github_untrusted");
    raw["headRefOid"] = json!("missing");
    assert!(scm::summarize(&raw).is_err());
}

#[tokio::test]
async fn invalid_url_fails_before_spawning_gh() {
    assert!(
        scm::fetch("--repo=malicious", std::path::Path::new("missing-cwd"))
            .await
            .unwrap_err()
            .to_string()
            .contains("链接")
    );
}
