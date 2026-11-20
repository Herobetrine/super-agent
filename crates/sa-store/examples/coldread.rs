//! Time the first (cold) ticket snapshot on a journal copy, the way the
//! desktop reader opens it. Read-only; no model or network calls.
//!
//! scripts\cargo.cmd run --release -p sa-store --example coldread -- --data DIR [--ticket ID] [--init]
//!
//! `--init` creates an empty journal schema in DIR (for synthetic fixtures,
//! see scripts/synthetic-journal.py) and exits.
use anyhow::{Context, Result, ensure};
use sa_store::Store;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data = None;
    let mut ticket = None;
    let mut init = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data" => data = Some(PathBuf::from(it.next().context("--data DIR")?)),
            "--ticket" => ticket = Some(it.next().context("--ticket ID")?.parse::<i64>()?),
            "--init" => init = true,
            other => anyhow::bail!("unknown option {other}"),
        }
    }
    let data = data.context("--data DIR is required")?;
    ensure!(
        data != sa_store::default_data_dir(),
        "refusing to run against the live journal; use a copy"
    );
    if init {
        Store::open(&data)?;
        println!("schema created in {}", data.display());
        return Ok(());
    }
    let started = Instant::now();
    let mut store = Store::open_read_only(&data)?;
    let opened = started.elapsed();
    let history = store.history("")?;
    let ticket = ticket.or_else(|| history.first().map(|t| t.id)).context("no ticket")?;
    let listed = started.elapsed();
    // The desktop reader's path: a bounded first load, then repeated loads
    // until the activity replay and the usage scan have caught up.
    let mut budgeted = Store::open_read_only(&data)?;
    let first = Instant::now();
    let mut snap = budgeted.snapshot_within(ticket, 0, std::time::Duration::from_millis(120))?;
    let shown = first.elapsed();
    let mut loads = 1;
    while snap.loading || snap.usage.pending {
        snap = budgeted.snapshot_within(ticket, 0, std::time::Duration::from_millis(120))?;
        loads += 1;
    }
    println!(
        "budgeted: first view {shown:?}, complete after {:?} ({loads} loads)",
        first.elapsed()
    );
    let first = Instant::now();
    let snap = store.snapshot(ticket, 0)?;
    let cold = first.elapsed();
    let second = Instant::now();
    let again = store.snapshot(ticket, 0)?;
    let warm = second.elapsed();
    let mut complete = again.clone();
    while complete.usage.pending {
        complete = store.snapshot(ticket, 0)?;
    }
    let budgeted_final = budgeted.snapshot_within(ticket, 0, std::time::Duration::from_millis(120))?;
    ensure!(
        budgeted_final == complete,
        "the budgeted load did not converge to the full snapshot"
    );
    println!("budgeted and full snapshots are identical");
    println!(
        "ticket {ticket}: open {:?}, history {:?}, unbudgeted first snapshot {:?} ({} events), second {:?} ({} events)",
        opened,
        listed - opened,
        cold,
        snap.events.len(),
        warm,
        again.events.len()
    );
    Ok(())
}
