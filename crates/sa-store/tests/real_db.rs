//! Smoke test against a copy of a real journal. Never point this at the live
//! database: set `SA_DB_COPY_DIR` to a directory holding a copied `hub.sqlite3`.
//!
//! `node scripts/cargo.cjs test -p sa-store --test real_db -- --ignored --nocapture`

use rusqlite::{Connection, OpenFlags};
use sa_store::{DB_FILE, Store};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
#[ignore = "needs SA_DB_COPY_DIR pointing at a copy of a real journal"]
fn real_journal_copy_reads_every_ticket() {
    let dir = PathBuf::from(std::env::var_os("SA_DB_COPY_DIR").expect("set SA_DB_COPY_DIR"));
    let ids: Vec<i64> = {
        let conn = Connection::open_with_flags(dir.join(DB_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let mut stmt = conn.prepare("SELECT id FROM tickets ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let started = Instant::now();
    let mut store = Store::open_read_only(&dir).unwrap();
    let t = Instant::now();
    let history = store.history("").unwrap();
    println!(
        "open+history: {} rows in {:?}; {} tickets total",
        history.len(),
        t.elapsed(),
        ids.len()
    );

    // Optional: dump every read as JSON lines for a parity diff against the JS Studio.
    let mut parity = std::env::var_os("SA_PARITY_OUT").map(|p| std::fs::File::create(p).unwrap());
    let mut slowest: (Duration, i64) = (Duration::ZERO, 0);
    let (mut events, mut activity, mut jobs, mut tasks) = (0usize, 0usize, 0usize, 0usize);
    for &ticket in &ids {
        let t0 = Instant::now();
        let first = store.snapshot(ticket, 0).unwrap();
        let cold = t0.elapsed();
        let t1 = Instant::now();
        let tail = store.snapshot(ticket, first.cursor).unwrap();
        let warm = t1.elapsed();
        assert!(tail.events.is_empty() || tail.events[0].seq > first.cursor);
        let again = store.snapshot(ticket, 0).unwrap();
        assert_eq!(again.events.len(), first.events.len());
        let t2 = Instant::now();
        let (mut pages, mut job_count, mut before) = (0, 0, None);
        loop {
            let page = store.jobs_page(ticket, before).unwrap();
            pages += 1;
            job_count += page.jobs.len();
            if !page.more {
                break;
            }
            before = page.next;
        }
        assert_eq!(job_count as i64, first.total_jobs);
        let jobs_time = t2.elapsed();
        let t3 = Instant::now();
        let act = store.activity_page(ticket, None).unwrap();
        let act_time = t3.elapsed();
        if let Some(next) = act.next.filter(|_| act.more) {
            store.activity_page(ticket, Some(next)).unwrap();
        }
        if let Some(out) = parity.as_mut() {
            use std::io::Write;
            let mut job_pages = vec![];
            let mut before = None;
            loop {
                let page = store.jobs_page(ticket, before).unwrap();
                let more = page.more;
                before = page.next;
                job_pages.push(page);
                if !more {
                    break;
                }
            }
            let mut activity_pages = vec![];
            let mut before = None;
            loop {
                let page = store.activity_page(ticket, before).unwrap();
                let more = page.more;
                before = page.next;
                activity_pages.push(page);
                if !more {
                    break;
                }
            }
            let line = serde_json::json!({"ticket": ticket, "snapshot": first, "tail": tail,
                "jobs": job_pages, "activity": activity_pages, "history": history});
            writeln!(out, "{line}").unwrap();
        }
        let payload = serde_json::to_string(&first).unwrap().len();
        println!(
            "ticket {ticket:>4} [{:<11}] events {:>3} (+{:>3} tail) activity {:>2}{} jobs {:>4} in {pages} pages, tasks {:>3}, \
             approvals {}, plan {}, json {:>8} B | snapshot cold {:>9.2?} tail {:>8.2?} jobs {:>8.2?} activity {:>8.2?}",
            first.ticket.state,
            first.events.len(),
            tail.events.len(),
            act.events.len(),
            if act.more { "+" } else { " " },
            job_count,
            first.native_activity.tasks.len(),
            first.approvals.len(),
            first.plan.len(),
            payload,
            cold,
            warm,
            jobs_time,
            act_time,
        );
        if cold > slowest.0 {
            slowest = (cold, ticket);
        }
        events += first.events.len();
        activity += act.events.len();
        jobs += job_count;
        tasks += first.native_activity.tasks.len();
    }
    println!(
        "TOTAL tickets {} events {events} activity(first pages) {activity} jobs {jobs} native tasks {tasks}; \
         slowest cold snapshot {:?} (ticket {}); all done in {:?}",
        ids.len(),
        slowest.0,
        slowest.1,
        started.elapsed()
    );
}
