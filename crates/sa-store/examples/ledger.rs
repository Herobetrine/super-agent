//! Read-only spending ledger and current-state report. No model or network calls.
use anyhow::{Context, Result, ensure};
use sa_store::{evaluation, ledger};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "只读用量账目（不运行模型，不上传数据，不写工单库）\n\
scripts\\cargo.cmd run -p sa-store --example ledger -- --data DATA_DIR [--ticket ID] --output new-report.json\n\
不传 --ticket 时汇总全部工单；带 --ticket 时输出该工单逐次执行明细。输出文件必须尚不存在。请对工单库副本运行。"
        );
        return Ok(());
    }
    ensure!(args.len() % 2 == 0, "每个选项必须有一个值；用 --help 查看用法");
    let mut options = BTreeMap::new();
    for pair in args.as_chunks::<2>().0 {
        ensure!(matches!(pair[0].as_str(), "--data" | "--ticket" | "--output"), "未知选项");
        ensure!(options.insert(pair[0].as_str(), pair[1].as_str()).is_none(), "选项不能重复");
    }
    let output = Path::new(options.get("--output").context("缺少 --output")?);
    let data = options.get("--data").map(PathBuf::from).unwrap_or_else(sa_store::default_data_dir);
    let ticket = options
        .get("--ticket")
        .map(|value| value.parse::<i64>().context("工单编号必须是正整数"))
        .transpose()?;
    if let Some(ticket) = ticket {
        let report = ledger::status_report(&data, Some(ticket))?;
        let conn = rusqlite::Connection::open_with_flags(
            data.join(sa_store::DB_FILE),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
        let detail = ledger::ticket_ledger(&conn, ticket)?;
        evaluation::write_new(output, &serde_json::json!({"summary": report, "ledger": detail}))?;
    } else {
        let report = ledger::status_report(&data, None)?;
        let cost = &report.api_cost;
        let totals: Vec<String> = cost.totals.iter().map(|t| format!("{} {}", t.amount, t.currency)).collect();
        println!(
            "已汇总 {} 张工单：{} 次执行中 {} 次报告了用量；按 API 牌价折算（价格表 {}，非实际账单）{} 次已折算{}：{}。",
            report.tickets.len(),
            report.coverage.attempts,
            report.coverage.reported_attempts,
            cost.price_table,
            cost.attempts_priced,
            if cost.partial { "，金额不完整" } else { "" },
            if totals.is_empty() { "金额未知".into() } else { totals.join("、") }
        );
        evaluation::write_new(output, &report)?;
    }
    Ok(())
}
