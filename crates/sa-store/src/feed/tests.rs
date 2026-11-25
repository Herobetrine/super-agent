use super::*;
use rusqlite::{Statement, StatementStatus};

pub(super) struct StatementSample {
    sql: String,
    plan: Vec<String>,
    steps: i32,
}

impl Reader<'_> {
    pub(super) fn record(
        &self,
        stmt: &Statement<'_>,
        sql: &str,
        args: &[&dyn ToSql],
    ) -> rusqlite::Result<()> {
        let steps = stmt.get_status(StatementStatus::VmStep);
        let plan = self
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map(args, |r| r.get(3))?
            .collect::<rusqlite::Result<_>>()?;
        self.trace.borrow_mut().push(StatementSample {
            sql: stmt.expanded_sql().unwrap(),
            steps,
            plan,
        });
        Ok(())
    }
}

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(crate::schema::SCHEMA).unwrap();
    db.execute_batch(
        "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'test','.',0,'claude',0);
        INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
        (1,1,NULL,'claude','',0,0,2,'running'),(2,1,1,'codex','',0,1,1,'done'),
        (3,1,1,'codex','',0,1,2,'running');",
    )
    .unwrap();
    db
}

fn write_rows(db: &Connection, n: i64, job: Option<i64>, turn: i64, kind: &str) {
    db.execute(
        "WITH RECURSIVE copies(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM copies WHERE i<?1)
        INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256)
        SELECT 0,1,?2,?3,?4,'{}','' FROM copies",
        params![n, job, turn, kind],
    )
    .unwrap();
}

fn reader(db: &Connection, after: i64) -> Reader<'_> {
    let end = db
        .query_row("SELECT max(seq) FROM events", [], |r| r.get(0))
        .unwrap();
    Reader::new(db, Indexes::detect(db).unwrap(), 1, after, end)
}

#[test]
fn sparse_page_work_is_bounded_in_both_directions() {
    let db = database();
    write_rows(&db, 20_000, Some(2), 1, "note");
    write_rows(&db, 20_000, Some(1), 1, "job.progress");
    write_rows(&db, 20_000, Some(1), 1, "native");
    write_rows(&db, 1, Some(1), 1, "note");
    write_rows(&db, 1, None, 1, "note");
    write_rows(&db, 1, Some(1), 2, "turn.admitted");
    // The resume lookup must also skip other jobs' admissions.
    write_rows(&db, 20_000, Some(2), 1, "turn.admitted");
    for after in [0, 1] {
        let reader = reader(&db, after);
        let fold = reader.fold(Some(1)).unwrap();
        assert_eq!(fold.resumed, 60_003);
        let page = reader.page(&fold, None).unwrap();
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [60_001, 60_002]
        );
        let steps: i64 = reader
            .trace
            .borrow()
            .iter()
            .map(|s| i64::from(s.steps))
            .sum();
        // One page's allowance includes all kind seeks, the open peer's
        // current round, the resume lookup, and payload fetches.
        assert!(
            steps <= PAGE as i64 * 64,
            "{steps} VM steps at after={after}"
        );
    }
}

#[test]
fn historical_ownership_uses_indexed_slices_or_one_legacy_scan() {
    let db = database();
    write_rows(&db, 1, Some(1), 1, "note");
    write_rows(&db, 1, None, 1, "note");
    write_rows(&db, 1, Some(1), 2, "turn.admitted");
    for index in ["events_ticket_job_kind", "events_ticket_kind"] {
        if index == "events_ticket_kind" {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        for after in [0, 1] {
            let reader = reader(&db, after);
            let fold = reader.fold(Some(1)).unwrap();
            reader.trace.borrow_mut().clear();
            reader.page(&fold, None).unwrap();
            let trace = reader.trace.borrow();
            if index == "events_ticket_kind" {
                assert_eq!(trace.len(), 1);
                assert!(
                    trace[0]
                        .plan
                        .iter()
                        .any(|p| p.contains("CORRELATED SCALAR SUBQUERY"))
                );
                assert!(
                    trace[0]
                        .plan
                        .iter()
                        .any(|p| p.contains("SEARCH pj USING INTEGER PRIMARY KEY"))
                );
                continue;
            }
            for owner in ["job IS 1", "job IS NULL", "job IS 3"] {
                let samples: Vec<_> = trace.iter().filter(|s| s.sql.contains(owner)).collect();
                assert!(!samples.is_empty(), "missing {owner}");
                for sample in samples {
                    assert!(
                        sample
                            .plan
                            .iter()
                            .any(|p| p.contains(&format!("INDEX {index} ("))),
                        "{:?}",
                        sample.plan
                    );
                }
            }
            assert!(
                trace
                    .iter()
                    .flat_map(|s| &s.plan)
                    .all(|p| !p.contains("INDEX events_ticket ("))
            );
        }
    }
}

#[test]
fn feed_work_is_independent_of_unrelated_open_jobs_on_both_paths() {
    for indexed in [true, false] {
        let db = database();
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        db.execute_batch("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(2,'other','.',0,'codex',0);
            INSERT INTO jobs(ticket,agent,body,writing,depth,state) VALUES(2,'codex','',0,1,'done');").unwrap();
        write_rows(&db, 20, Some(2), 1, "note");
        write_rows(&db, 1, Some(1), 2, "note");
        write_rows(&db, 1, Some(1), 2, "turn.admitted");
        let cost = |after| {
            let r = reader(&db, after);
            let page = r.page(&r.fold(Some(1)).unwrap(), None).unwrap();
            assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), [21]);
            let trace = r.trace.into_inner();
            let candidates = trace
                .iter()
                .find(|s| s.sql.contains("WITH RECURSIVE referenced"));
            assert_eq!(candidates.is_some(), indexed);
            if let Some(query) = candidates {
                for fragment in [
                    "INDEX jobs_ticket_id",
                    "INDEX events_ticket_job_kind (ticket=? AND job>?)",
                    "SEARCH j USING INTEGER PRIMARY KEY",
                ] {
                    assert!(
                        query.plan.iter().any(|p| p.contains(fragment)),
                        "{:?}",
                        query.plan
                    );
                }
            }
            trace.iter().map(|s| i64::from(s.steps)).sum::<i64>()
        };
        let before = [cost(0), cost(1)];
        db.execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000)
            INSERT INTO jobs(ticket,agent,body,writing,depth,state) SELECT 2,'codex','',0,1,
            CASE x%2 WHEN 0 THEN 'interrupted' ELSE 'running' END FROM n",
        )
        .unwrap();
        assert_eq!(before, [cost(0), cost(1)]);
    }
}

#[test]
fn missing_kind_index_uses_ticket_index_or_leaves_the_planner_free() {
    let db = database();
    db.execute_batch("DROP INDEX events_ticket_job_kind; DROP INDEX events_ticket_kind;")
        .unwrap();
    write_rows(&db, 1, Some(1), 1, "turn.admitted");
    write_rows(&db, 1, Some(1), 2, "delta");
    for ticket_index in [true, false] {
        let name = if ticket_index {
            "events_ticket"
        } else {
            db.execute_batch(
                "DROP INDEX events_ticket; CREATE INDEX custom_ticket_seq ON events(ticket,seq);",
            )
            .unwrap();
            "custom_ticket_seq"
        };
        for after in [0, 1] {
            let r = reader(&db, after);
            assert_eq!(r.fold(Some(1)).unwrap().resumed, 1);
            assert_eq!(r.deltas(1, 2, 2).unwrap()[0].seq, 2);
            let trace = r.trace.into_inner();
            for sample in trace {
                assert_eq!(sample.sql.contains("INDEXED"), ticket_index);
                assert!(
                    sample
                        .plan
                        .iter()
                        .any(|p| p.contains(&format!("INDEX {name} (ticket=?"))),
                    "{:?}",
                    sample.plan
                );
            }
        }
    }
}

#[test]
fn open_round_reads_turn_from_the_covering_index() {
    let db = database();
    write_rows(&db, 20_000, Some(3), 1, "note");
    db.execute_batch(
        "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256)
        VALUES(0,1,3,NULL,'note','{}','')",
    )
    .unwrap();
    write_rows(&db, 1, Some(3), 2, "note"); // Delivered while queued.
    write_rows(&db, 1, Some(3), 2, "turn.admitted");
    write_rows(&db, 1, Some(3), 2, "note");
    write_rows(&db, 1, Some(1), 2, "turn.admitted");
    for after in [0, 1] {
        let r = reader(&db, after);
        let page = r.page(&r.fold(Some(1)).unwrap(), None).unwrap();
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [20_001, 20_002, 20_004]
        );
        let trace = r.trace.borrow();
        let notes = trace
            .iter()
            .find(|s| s.sql.contains("job IS 3") && s.sql.contains("kind='note'"))
            .unwrap();
        assert!(
            notes
                .plan
                .iter()
                .any(|p| p.contains("COVERING INDEX events_ticket_job_kind")),
            "{:?}",
            notes.plan
        );
        // One indexed turn check per old row, plus the three kept rows and seek overhead.
        assert!(notes.steps <= 6 * 20_000 + 512, "{} steps", notes.steps);
    }
}

#[test]
fn full_latest_activity_page_skips_historical_open_rounds() {
    use crate::progress::{Entry, ScanRow};
    let mut entry = Entry::default();
    let fold = Fold {
        root: Some(1),
        resumed: 1001,
    };
    for seq in 1..=fold.resumed + PAGE as i64 - 1 {
        entry.absorb(ScanRow {
            seq,
            job: Some(3),
            turn: Some(if seq < 1000 { 1 } else { 2 }),
            ts: 0.0,
            kind: "job.progress".into(),
            agent: None,
            payload: None,
            summary: Some("report".into()),
            status: None,
        });
    }
    let db = database();
    // A separate raw row allows the mixed case to reach 180 with only
    // 179 supplemental rows after the resume point.
    db.execute_batch(
        "INSERT INTO events(seq,ts,ticket,job,turn,kind,payload,sha256)
        VALUES(1181,0,1,1,2,'note','{}','')",
    )
    .unwrap();
    for indexed in [true, false] {
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        let read = |after, end, fold: &Fold| {
            entry.feed_turn_checks.set(0);
            let r = Reader::new(&db, Indexes::detect(&db).unwrap(), 1, after, end);
            let mut rows = r.page(fold, Some(&entry)).unwrap();
            rows.sort_by_key(|e| e.seq);
            let seqs = page_slice(&rows, after)
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>();
            let queries = r
                .trace
                .borrow()
                .iter()
                .filter(|s| {
                    s.sql.contains("WITH RECURSIVE referenced")
                        || s.sql.starts_with("SELECT id,state,turn FROM jobs")
                })
                .count();
            (seqs, entry.feed_turn_checks.get(), queries)
        };
        let (latest, checks, queries) = read(0, 1180, &fold);
        assert_eq!(latest, (1001..=1180).collect::<Vec<_>>());
        assert_eq!((checks, queries), (0, 0));
        let mixed = Fold {
            root: Some(1),
            resumed: 1002,
        };
        let (latest, checks, queries) = read(0, 1181, &mixed);
        assert_eq!(latest, (1002..=1181).collect::<Vec<_>>());
        assert_eq!((checks, queries), (0, 0));
        let (short, checks, queries) = read(0, 1179, &fold);
        assert_eq!(short, (1000..=1179).collect::<Vec<_>>());
        assert!(checks > 0 && queries == 1);
        let (older, checks, queries) = read(1, 1181, &fold);
        assert_eq!(older, (1000..=1179).collect::<Vec<_>>());
        assert!(checks > 0 && queries == 1);
        let (later, checks, queries) = read(fold.resumed, 1181, &fold);
        assert_eq!(later, (1002..=1181).collect::<Vec<_>>());
        assert_eq!((checks, queries), (0, 0));
    }
}

#[test]
fn incremental_delta_seek_skips_earlier_turn_admissions() {
    fn cost(indexed: bool, prior_turns: i64) -> i32 {
        let db = database();
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        db.execute(
            "WITH RECURSIVE n(t) AS (VALUES(1) UNION ALL SELECT t+1 FROM n WHERE t<?1)
            INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256)
            SELECT 0,1,1,t,'turn.admitted','{}','' FROM n",
            [prior_turns],
        )
        .unwrap();
        let turn = prior_turns + 1;
        write_rows(&db, 1, Some(1), turn, "turn.admitted");
        write_rows(&db, 1, Some(1), turn, "delta");
        let first = db.last_insert_rowid();
        write_rows(&db, 1, Some(1), turn, "turn.admitted");
        write_rows(&db, 1, Some(1), turn, "delta");
        let last = db.last_insert_rowid();
        let r = reader(&db, first);
        let page = r.deltas(1, turn, last).unwrap();
        assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), [last]);
        let trace = r.trace.into_inner();
        assert_eq!(trace.len(), 1);
        assert!(!trace[0].sql.contains("turn.admitted"));
        trace[0].steps
    }
    for indexed in [true, false] {
        assert_eq!(cost(indexed, 10), cost(indexed, 20_000));
    }
}

#[test]
fn initial_delta_work_depends_only_on_deltas_after_the_previous_turn_boundary() {
    fn measure(indexed: bool, old: i64, tail: i64, concurrent: i64) -> i32 {
        let db = database();
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        write_rows(&db, 1, Some(1), 1, "turn.admitted");
        write_rows(&db, old, Some(1), 1, "delta");
        write_rows(&db, 1, Some(2), 2, "turn.admitted");
        write_rows(&db, old, Some(2), 2, "delta");
        write_rows(&db, 1, Some(1), 2, "turn.admitted"); // Initial lower bound.
        write_rows(&db, tail, Some(1), 2, "delta");
        write_rows(&db, 1, Some(1), 3, "turn.admitted");
        write_rows(&db, 1, Some(1), 3, "delta");
        let first_delta = db.last_insert_rowid();
        write_rows(&db, 1, Some(1), 3, "retry");
        write_rows(&db, 1, Some(1), 3, "turn.admitted");
        write_rows(&db, concurrent, Some(2), 2, "delta");
        write_rows(&db, 1, Some(1), 3, "delta");
        let last = db.last_insert_rowid();
        let initial = reader(&db, 0);
        let deltas = initial.deltas(1, 3, last).unwrap();
        assert_eq!(
            deltas.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [first_delta, last]
        );
        let later = reader(&db, first_delta).deltas(1, 3, last).unwrap();
        assert_eq!(later.iter().map(|e| e.seq).collect::<Vec<_>>(), [last]);
        initial.trace.into_inner().iter().map(|s| s.steps).sum()
    }
    for indexed in [true, false] {
        let small = measure(indexed, 10, 1, 1);
        assert_eq!(small, measure(indexed, 20_000, 1, 1));
        assert!(measure(indexed, 10, 400, 1) > small);
        assert!(measure(indexed, 10, 1, 400) > small);
    }
}

#[test]
fn delta_read_without_matching_admission_keeps_the_original_cursor_range() {
    let db = database();
    write_rows(&db, 1, Some(1), 1, "turn.admitted");
    write_rows(&db, 1, Some(1), 2, "delta");
    write_rows(&db, 1, Some(2), 2, "turn.admitted");
    write_rows(&db, 1, Some(1), 2, "delta");
    write_rows(&db, 1, Some(1), 2, "delta");
    for indexed in [true, false] {
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        let read = |after| {
            reader(&db, after)
                .deltas(1, 2, 4)
                .unwrap()
                .into_iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(read(0), [2, 4]);
        assert_eq!(read(2), [4]);
    }
}
