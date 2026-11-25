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
        let page = reader.page(&fold).unwrap();
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
fn historical_ownership_plans_pin_the_available_index() {
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
            reader.page(&fold).unwrap();
            let trace = reader.trace.borrow();
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
fn first_admission_bounds_delta_work_and_keeps_both_attempts() {
    fn measure(indexed: bool, old: i64, concurrent: i64) -> i32 {
        let db = database();
        if !indexed {
            db.execute_batch("DROP INDEX events_ticket_job_kind")
                .unwrap();
        }
        write_rows(&db, 1, Some(1), 1, "turn.admitted");
        write_rows(&db, old, Some(1), 1, "delta");
        write_rows(&db, 1, Some(2), 2, "turn.admitted");
        write_rows(&db, old, Some(2), 2, "delta");
        write_rows(&db, 1, Some(1), 2, "turn.admitted");
        write_rows(&db, 1, Some(1), 2, "delta");
        let first_delta = db.last_insert_rowid();
        write_rows(&db, 1, Some(1), 2, "retry");
        write_rows(&db, 1, Some(1), 2, "turn.admitted");
        write_rows(&db, concurrent, Some(2), 2, "delta");
        write_rows(&db, 1, Some(1), 2, "delta");
        let last = db.last_insert_rowid();
        let initial = reader(&db, 0);
        let deltas = initial.deltas(1, 2, last).unwrap();
        assert_eq!(
            deltas.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [first_delta, last]
        );
        let later = reader(&db, first_delta).deltas(1, 2, last).unwrap();
        assert_eq!(later.iter().map(|e| e.seq).collect::<Vec<_>>(), [last]);
        initial.trace.into_inner().iter().map(|s| s.steps).sum()
    }
    for indexed in [true, false] {
        let small = measure(indexed, 10, 1);
        assert_eq!(small, measure(indexed, 20_000, 1));
        assert!(measure(indexed, 10, 400) > small);
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
