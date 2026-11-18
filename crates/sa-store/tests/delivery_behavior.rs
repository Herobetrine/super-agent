use rusqlite::{Connection, params};
use sa_store::{
    Store,
    delivery::{self, DeliveryFile, DeliverySnapshot},
};
use std::{fs, path::PathBuf};

struct Fixture {
    data: tempfile::TempDir,
    project: PathBuf,
    db: Connection,
    manifest: String,
}

impl Fixture {
    fn new(expected_file: Option<&[u8]>) -> Self {
        let data = tempfile::tempdir().unwrap();
        let project = data.path().join("project");
        fs::create_dir(&project).unwrap();
        drop(Store::open(data.path()).unwrap());
        let db = Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
        db.execute_batch(delivery::SCHEMA).unwrap();
        db.execute("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'fixture',?,0,'codex',1)", [project.to_str().unwrap()]).unwrap();
        db.execute_batch(
            "INSERT INTO delivery_contracts VALUES(1,2);
            INSERT INTO delivery_requirements VALUES(1,1);
            INSERT INTO delivery_gate_policies VALUES(1,1,1,1);",
        )
        .unwrap();
        db.execute("INSERT INTO delivery_requirement_revisions(requirement,revision,contract_revision,text,path,expected_sha256,source,created) VALUES(1,1,1,'criterion',?,?,'operator',1)",
            params![expected_file.map(|_| "answer.txt"), expected_file.map(delivery::sha256)]).unwrap();
        let objects = data.path().join(delivery::OBJECT_DIRECTORY);
        fs::create_dir(&objects).unwrap();
        let files: Vec<_> = [
            ("answer.txt", b"expected".as_slice()),
            ("check.script", b"verify"),
            ("library.txt", b"input"),
        ]
        .into_iter()
        .map(|(path, bytes)| {
            let sha256 = delivery::sha256(bytes);
            fs::write(project.join(path), bytes).unwrap();
            fs::write(objects.join(&sha256), bytes).unwrap();
            DeliveryFile {
                path: path.into(),
                sha256,
                bytes: bytes.len() as u64,
            }
        })
        .collect();
        db.execute("INSERT INTO delivery_behavior_bindings(id,requirement,requirement_revision,contract_revision,argv,verifier_files,created) VALUES(1,1,1,2,'[\"runtime\",\"check.script\"]',?,1)",
            [serde_json::to_string(&files[1..2]).unwrap()]).unwrap();
        let frozen = DeliverySnapshot {
            schema_version: 2,
            id: 1,
            ticket: 1,
            contract_revision: 2,
            created_at: 1.,
            files,
            requirements: delivery::requirements(&db, 1).unwrap(),
            evidence: Vec::new(),
            origin: None,
        };
        let payload = serde_json::to_string(&frozen).unwrap();
        let manifest = delivery::sha256(payload.as_bytes());
        db.execute(
            "INSERT INTO delivery_snapshots VALUES(1,1,2,1,?,?)",
            params![payload, manifest],
        )
        .unwrap();
        Self {
            data,
            project,
            db,
            manifest,
        }
    }

    fn start(&self, id: i64) {
        let env = serde_json::json!({"runner":delivery::BEHAVIOR_RUNNER,"cwd":"fresh_manifest_directory","environment_policy":delivery::BEHAVIOR_ENVIRONMENT_POLICY,
            "os":"test","arch":"test","program":"runtime","program_sha256":delivery::sha256(b"runtime"),"path_sha256":delivery::sha256(b"path"),
            "argv_sha256":delivery::sha256(br#"["runtime","check.script"]"#),"timeout_ms":50});
        self.db
            .execute(
                "INSERT INTO delivery_behavior_attempts VALUES(?,1,1,1,1,1,2,?,1,?)",
                params![id, self.manifest, env.to_string()],
            )
            .unwrap();
    }

    fn finish(&self, id: i64, status: &str, code: i32) {
        self.db
            .execute(
                "INSERT INTO delivery_behavior_results VALUES(?,?,?,'fixture',2)",
                params![id, status, code],
            )
            .unwrap();
    }

    fn assess_ready(&self) {
        self.db
            .execute(
                "INSERT INTO delivery_assessments VALUES(1,1,2,1,1,?,'ready',3,'[]','checked')",
                [&self.manifest],
            )
            .unwrap();
        self.db
            .execute(
                "INSERT INTO delivery_assessment_behavior VALUES(1,?)",
                [delivery::behavior_basis(&self.db, 1, 1).unwrap()],
            )
            .unwrap();
    }

    fn read(&self) -> delivery::DeliverySummary {
        delivery::read_summary(self.data.path(), 1).unwrap()
    }

    fn record_acceptance(&self) -> i64 {
        let summary = self.read();
        let assessment = summary.gate.acceptable_assessment().unwrap();
        let basis =
            delivery::behavior_basis(&self.db, assessment.ticket, assessment.snapshot_id).unwrap();
        let digest = delivery::acceptance_digest(assessment, &basis).unwrap();
        self.db.execute("INSERT INTO delivery_acceptances(ticket,assessment_id,assessment_status,assessment_sha256,created) VALUES(?,?,?,?,4)",
            params![assessment.ticket,assessment.id,assessment.status,digest]).unwrap();
        self.db.last_insert_rowid()
    }

    fn revise_manifest(&mut self, change: impl FnOnce(&mut DeliverySnapshot)) {
        let payload: String = self
            .db
            .query_row(
                "SELECT payload FROM delivery_snapshots WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut frozen: DeliverySnapshot = serde_json::from_str(&payload).unwrap();
        change(&mut frozen);
        let payload = serde_json::to_string(&frozen).unwrap();
        self.manifest = delivery::sha256(payload.as_bytes());
        self.db
            .execute(
                "UPDATE delivery_snapshots SET payload=?,sha256=?,contract_revision=? WHERE id=1",
                params![payload, self.manifest, frozen.contract_revision],
            )
            .unwrap();
    }
}

#[test]
fn latest_attempt_controls_gate_even_if_an_older_attempt_finishes_later() {
    let fixture = Fixture::new(None);
    fixture.start(1);
    fixture.start(2);
    fixture.finish(2, "pass", 0);
    fixture.assess_ready();
    fixture.finish(1, "fail", 1);
    let summary = fixture.read();
    assert_eq!(summary.gate.status, "ready");
    assert_eq!(
        summary.requirements[0]
            .behavior
            .as_ref()
            .unwrap()
            .receipt
            .as_ref()
            .unwrap()
            .id,
        2
    );
    assert_eq!(
        delivery::behavior_receipt(&fixture.db, 1).unwrap().status,
        "fail"
    );
    fixture.start(3);
    let summary = fixture.read();
    assert_eq!(summary.requirements[0].status, "unknown");
    assert_eq!(summary.gate.status, "stale");
    fixture.finish(3, "pass", 0);
    let summary = fixture.read();
    assert_eq!(summary.requirements[0].status, "pass");
    assert_eq!(summary.gate.status, "stale");
}

#[test]
fn earlier_runner_pass_remains_auditable_and_requires_a_rerun() {
    let fixture = Fixture::new(None);
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    fixture.assess_ready();
    fixture.db.execute("UPDATE delivery_behavior_attempts SET environment=json_set(environment,'$.runner','snapshot-argv/1','$.environment_policy','host-path-private-home/1')",[]).unwrap();
    let summary = fixture.read();
    let requirement = &summary.requirements[0];
    assert_eq!(requirement.status, "stale");
    assert_eq!(
        requirement
            .behavior
            .as_ref()
            .unwrap()
            .receipt
            .as_ref()
            .unwrap()
            .status,
        "pass"
    );
    assert_eq!(
        summary.gate.latest_assessment.as_ref().unwrap().status,
        "ready"
    );
    assert_eq!(summary.gate.status, "blocked");
    fixture.start(2);
    fixture.finish(2, "pass", 0);
    let summary = fixture.read();
    assert_eq!(summary.requirements[0].status, "pass");
    assert_eq!(summary.gate.status, "stale");
}

#[test]
fn all_frozen_inputs_and_the_optional_file_criterion_are_checked() {
    for (expected, frozen_status) in [
        (b"expected".as_slice(), "pass"),
        (b"other".as_slice(), "fail"),
    ] {
        let fixture = Fixture::new(Some(expected));
        fixture.start(1);
        fixture.finish(1, "pass", 0);
        fixture.assess_ready();
        let summary = fixture.read();
        assert_eq!(summary.requirements[0].status, frozen_status);
        assert_eq!(summary.gate.status == "ready", frozen_status == "pass");
        fs::write(fixture.project.join("library.txt"), b"changed").unwrap();
        let summary = fixture.read();
        assert_eq!(summary.requirements[0].status, "stale");
        assert_eq!(
            summary.latest_snapshot.unwrap().requirements[0].status,
            frozen_status
        );
        let object = fixture
            .data
            .path()
            .join(delivery::OBJECT_DIRECTORY)
            .join(delivery::sha256(b"input"));
        fs::remove_file(object).unwrap();
        assert_ne!(fixture.read().gate.status, "ready");
    }
}

#[test]
fn malformed_or_mismatched_receipts_and_binding_corruption_fail_closed() {
    for sql in [
        "UPDATE delivery_behavior_results SET exit_code=1",
        "UPDATE delivery_behavior_results SET detail=zeroblob(1024)",
        "UPDATE delivery_behavior_results SET status='pending'",
        "UPDATE delivery_behavior_attempts SET environment='{}'",
        "UPDATE delivery_behavior_attempts SET manifest_sha256=lower(hex(zeroblob(32)))",
        "UPDATE delivery_behavior_bindings SET argv='[]'",
        "DELETE FROM delivery_assessment_behavior",
    ] {
        let fixture = Fixture::new(Some(b"expected"));
        fixture.start(1);
        fixture.finish(1, "pass", 0);
        fixture.assess_ready();
        assert_eq!(fixture.read().gate.status, "ready");
        fixture.db.execute_batch(sql).unwrap();
        assert_ne!(fixture.read().gate.status, "ready", "{sql}");
    }
    let mut fixture = Fixture::new(Some(b"expected"));
    fixture.revise_manifest(|frozen| frozen.schema_version = 1);
    assert!(delivery::read_snapshot(fixture.data.path(), 1, 1).is_err());
}

#[test]
fn a_passing_exit_cannot_validate_a_replaced_or_omitted_verifier() {
    for omitted in [true, false] {
        let mut fixture = Fixture::new(Some(b"expected"));
        let replacement = b"weaker";
        let hash = delivery::sha256(replacement);
        fs::write(
            fixture
                .data
                .path()
                .join(delivery::OBJECT_DIRECTORY)
                .join(&hash),
            replacement,
        )
        .unwrap();
        fixture.revise_manifest(|frozen| {
            if omitted {
                frozen.files.remove(1);
            } else {
                frozen.files[1].sha256 = hash;
                frozen.files[1].bytes = replacement.len() as u64;
            }
        });
        fixture.start(1);
        fixture.finish(1, "pass", 0);
        fixture.assess_ready();
        let summary = fixture.read();
        let requirement = &summary.latest_snapshot.as_ref().unwrap().requirements[0];
        assert_eq!(requirement.status, "stale");
        assert_eq!(
            requirement
                .behavior
                .as_ref()
                .unwrap()
                .receipt
                .as_ref()
                .unwrap()
                .status,
            "pass"
        );
        assert_ne!(summary.gate.status, "ready");
    }
}

#[test]
fn revised_text_cannot_fall_back_to_a_passing_file_hash() {
    let mut fixture = Fixture::new(Some(b"expected"));
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    fixture.assess_ready();
    fixture.db.execute_batch("UPDATE delivery_contracts SET revision=3;
        INSERT INTO delivery_requirement_revisions SELECT requirement,2,3,'new requirement',path,expected_sha256,job,source,3 FROM delivery_requirement_revisions WHERE revision=1;").unwrap();
    let summary = fixture.read();
    assert_eq!(summary.requirements[0].status, "stale");
    assert_eq!(summary.gate.status, "stale");
    let requirements = delivery::requirements(&fixture.db, 1).unwrap();
    fixture.revise_manifest(|frozen| {
        frozen.contract_revision = 3;
        frozen.requirements = requirements;
    });
    let summary = fixture.read();
    let frozen = summary.latest_snapshot.as_ref().unwrap();
    assert_eq!(frozen.requirements[0].status, "stale");
    assert_eq!(
        delivery::assess_snapshot(&summary.requirements, frozen).0,
        "blocked"
    );
}

#[test]
fn a_serialized_pass_cannot_replace_an_execution_record() {
    let mut fixture = Fixture::new(Some(b"expected"));
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    let receipt = delivery::behavior_receipt(&fixture.db, 1).unwrap();
    fixture.revise_manifest(|frozen| {
        frozen.requirements[0].behavior.as_mut().unwrap().receipt = Some(receipt);
        frozen.requirements[0].status = "pass".into();
    });
    fixture
        .db
        .execute_batch(
            "DELETE FROM delivery_behavior_results; DELETE FROM delivery_behavior_attempts;",
        )
        .unwrap();
    fixture.assess_ready();
    let summary = fixture.read();
    let requirement = &summary.latest_snapshot.as_ref().unwrap().requirements[0];
    assert_eq!(requirement.status, "unknown");
    assert!(requirement.behavior.as_ref().unwrap().receipt.is_none());
    assert_eq!(summary.gate.status, "blocked");
}

#[test]
fn acceptance_is_historical_after_a_rerun_or_an_identical_new_assessment() {
    let fixture = Fixture::new(None);
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    fixture.assess_ready();
    let id = fixture.record_acceptance();
    assert_eq!(fixture.read().gate.acceptance.unwrap().id, id);
    fixture.start(2);
    let gate = fixture.read().gate;
    assert_eq!(gate.status, "stale");
    assert!(gate.acceptance.is_none());
    assert_eq!(gate.latest_acceptance.unwrap().assessment_id, 1);
    fixture.finish(2, "pass", 0);
    fixture.db.execute_batch("INSERT INTO delivery_assessments SELECT 2,ticket,contract_revision,snapshot_id,policy_revision,manifest_sha256,status,created,unmet,reason FROM delivery_assessments WHERE id=1;").unwrap();
    fixture
        .db
        .execute(
            "INSERT INTO delivery_assessment_behavior VALUES(2,?)",
            [delivery::behavior_basis(&fixture.db, 1, 1).unwrap()],
        )
        .unwrap();
    let gate = fixture.read().gate;
    assert_eq!(gate.status, "ready");
    assert!(gate.acceptance.is_none());
    assert_eq!(gate.latest_acceptance.unwrap().id, id);
    let current = fixture.record_acceptance();
    assert_ne!(current, id);
    fixture
        .db
        .execute(
            "UPDATE delivery_assessments SET reason='different assessment' WHERE id=2",
            [],
        )
        .unwrap();
    let gate = fixture.read().gate;
    assert!(gate.acceptance.is_none());
    assert!(gate.latest_acceptance.is_none());
}

#[test]
fn accepting_partial_preserves_unmet_scope_and_expires_if_that_scope_changes() {
    let mut fixture = Fixture::new(None);
    fixture
        .db
        .execute_batch("INSERT INTO delivery_requirements(id,ticket) VALUES(2,1),(3,1);")
        .unwrap();
    for (id, path, expected) in [
        (2, "answer.txt", b"wrong".as_slice()),
        (3, "library.txt", b"input".as_slice()),
    ] {
        fixture.db.execute("INSERT INTO delivery_requirement_revisions(requirement,revision,contract_revision,text,path,expected_sha256,source,created) VALUES(?,1,2,'criterion',?,?,'operator',1)",
            params![id,path,delivery::sha256(expected)]).unwrap();
    }
    let requirements = delivery::requirements(&fixture.db, 1).unwrap();
    fixture.revise_manifest(|frozen| frozen.requirements = requirements);
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    let summary = fixture.read();
    let (status, unmet, reason) = delivery::assess_snapshot(
        &summary.requirements,
        summary.latest_snapshot.as_ref().unwrap(),
    );
    assert_eq!(status, "partial");
    fixture
        .db
        .execute(
            "INSERT INTO delivery_assessments VALUES(1,1,2,1,1,?,'partial',3,?,?)",
            params![
                fixture.manifest,
                serde_json::to_string(&unmet).unwrap(),
                reason
            ],
        )
        .unwrap();
    fixture
        .db
        .execute(
            "INSERT INTO delivery_assessment_behavior VALUES(1,?)",
            [delivery::behavior_basis(&fixture.db, 1, 1).unwrap()],
        )
        .unwrap();
    fixture.record_acceptance();
    let gate = fixture.read().gate;
    assert_eq!(gate.status, "partial");
    assert_eq!(gate.unmet, unmet);
    assert_eq!(gate.acceptance.unwrap().status, "partial");

    let object = fixture
        .data
        .path()
        .join(delivery::OBJECT_DIRECTORY)
        .join(delivery::sha256(b"verify"));
    fs::remove_file(object).unwrap();
    let gate = fixture.read().gate;
    assert_eq!(gate.status, "partial");
    assert_eq!(gate.unmet.len(), 2);
    assert!(gate.acceptable_assessment().is_none());
    assert!(gate.acceptance.is_none());
    assert_eq!(gate.latest_acceptance.unwrap().assessment_id, 1);
}

#[test]
fn readonly_ledger_without_acceptance_table_keeps_the_assessment_unaccepted() {
    let fixture = Fixture::new(None);
    fixture.start(1);
    fixture.finish(1, "pass", 0);
    fixture.assess_ready();
    fixture
        .db
        .execute("DROP TABLE delivery_acceptances", [])
        .unwrap();
    let gate = fixture.read().gate;
    assert_eq!(gate.status, "ready");
    assert!(gate.acceptance.is_none());
    assert!(gate.latest_acceptance.is_none());
    assert_eq!(
        fixture
            .db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='delivery_acceptances'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}
