//! Controlled trials over a fixed task suite (architecture §13, P5).
//!
//! `--validate` runs no model: it proves every task's hidden acceptance fails
//! on the fixture and passes on the reference solution. `--run` starts real
//! harnesses and is billable, so it requires `--billable`. `--aggregate`
//! summarizes existing run records.
use anyhow::{Context, Result, bail, ensure};
use sa_adapters::sys::Sys;
use sa_engine::{EngineOptions, trials};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const HELP: &str = "受控试验（任务集 eval/suites/core-v1/suite.json）\n\
  校验任务集（不调用模型）：trial --suite SUITE --validate --output new.json\n\
  运行试验（计费）：trial --suite SUITE --run --variants variants.json [--tasks a,b] [--repeat 3] --out DIR --billable\n\
  汇总记录：trial --suite SUITE --aggregate DIR --output new.json\n\
每次运行使用新的临时项目和工单库，不读写真实工单库。输出文件必须尚不存在。";

fn write_new(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    sa_store::evaluation::write_new(path, value)
}

fn records(dir: &Path) -> Result<Vec<trials::RunRecord>> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir.join("runs")).context("缺少 runs 目录")? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            out.push(serde_json::from_slice(&std::fs::read(&path)?)?);
        }
    }
    out.sort_by(|a: &trials::RunRecord, b| (&a.task, &a.variant.id, a.repetition).cmp(&(&b.task, &b.variant.id, b.repetition)));
    Ok(out)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let mut options: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = vec![];
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--validate" | "--run" | "--billable" => flags.push(arg),
            "--suite" | "--output" | "--variants" | "--tasks" | "--repeat" | "--out" | "--aggregate" => {
                let value = iter.next().context("选项缺少值")?;
                ensure!(options.insert(arg, value).is_none(), "选项不能重复");
            }
            other => bail!("未知选项 {other}"),
        }
    }
    let suite = trials::load(Path::new(options.get("--suite").context("缺少 --suite")?))?;
    if flags.iter().any(|f| f == "--validate") {
        let output = PathBuf::from(options.get("--output").context("缺少 --output")?);
        let report = trials::validate(&suite)?;
        for task in &report {
            println!("{}: {}（{}）", task.task, if task.valid { "有效" } else { "无效" }, task.reason);
        }
        write_new(&output, &report)?;
        ensure!(report.iter().all(|t| t.valid), "任务集中有无效任务");
        return Ok(());
    }
    if let Some(dir) = options.get("--aggregate") {
        let output = PathBuf::from(options.get("--output").context("缺少 --output")?);
        let report = trials::aggregate(&records(Path::new(dir))?)?;
        write_new(&output, &report)?;
        println!("已汇总 {} 组。", report.groups.len());
        return Ok(());
    }
    ensure!(flags.iter().any(|f| f == "--run"), "请选择 --validate、--run 或 --aggregate");
    ensure!(flags.iter().any(|f| f == "--billable"), "运行试验会调用真实模型并产生费用，请加 --billable 确认");
    let variants: Vec<trials::Variant> = serde_json::from_slice(&std::fs::read(
        options.get("--variants").context("缺少 --variants")?,
    )?)?;
    ensure!(!variants.is_empty(), "至少需要一个变体");
    let repeat: u32 = options.get("--repeat").map(|r| r.parse()).transpose()?.unwrap_or(3);
    let wanted: Option<Vec<&str>> = options.get("--tasks").map(|t| t.split(',').collect());
    let out = PathBuf::from(options.get("--out").context("缺少 --out")?);
    std::fs::create_dir_all(out.join("runs"))?;
    let sys = Sys::from_process();
    let adapters = move || sa_adapters::all(sys.clone());
    let engine_options = |efficiency| EngineOptions {
        efficiency: sa_store::efficiency::shared(efficiency),
        ..Default::default()
    };
    let harness = trials::Harness { adapters: &adapters, options: &engine_options, poll: Duration::from_millis(500) };
    // Alternate variants within each repetition so drift affects all of them.
    for repetition in 1..=repeat {
        for task in &suite.suite.tasks {
            if wanted.as_ref().is_some_and(|w| !w.contains(&task.id.as_str())) {
                continue;
            }
            for variant in &variants {
                let name = format!("{}-{}-{repetition}", task.id, variant.id);
                let record_path = out.join("runs").join(format!("{name}.json"));
                if record_path.exists() {
                    continue;
                }
                let work = out.join("work").join(&name);
                println!("运行 {name} …");
                let record = trials::run(&harness, &suite, task, variant, repetition, &work).await?;
                println!("  {} · {} ms · 根任务 {}", record.verdict, record.elapsed_ms, record.root_state);
                write_new(&record_path, &record)?;
            }
        }
    }
    let report = trials::aggregate(&records(&out)?)?;
    let report_path = out.join(format!("report-{}.json", std::process::id()));
    write_new(&report_path, &report)?;
    println!("报告：{}", report_path.display());
    Ok(())
}
