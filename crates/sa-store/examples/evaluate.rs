//! Local journal evaluation and report comparison. No model or network calls.
use anyhow::{Context, Result, bail, ensure};
use sa_store::evaluation::{self, Dataset, Report};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

fn read<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let mut bytes = vec![];
    File::open(path)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024 * 1024, "输入 JSON 超过 64 MiB");
    serde_json::from_slice(&bytes).context("输入 JSON 无效")
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "本地只读评测（不运行模型，不上传数据）\n\
scripts\\cargo.cmd run -p sa-store --example evaluate -- --data DATA_DIR --ticket ID [--dataset dataset.json --case CASE_ID] --output new-report.json\n\
scripts\\cargo.cmd run -p sa-store --example evaluate -- --left report-a.json --right report-b.json --output new-comparison.json\n\
不传 --data 时使用 SUPERAGENT_DATA 或默认工单库。输出文件必须尚不存在。文件断言检查当前工作区，非历史快照。"
        );
        return Ok(());
    }
    ensure!(
        args.len() % 2 == 0,
        "每个选项必须有一个值；用 --help 查看用法"
    );
    let mut options = BTreeMap::new();
    for pair in args.as_chunks::<2>().0 {
        ensure!(
            matches!(
                pair[0].as_str(),
                "--data" | "--ticket" | "--dataset" | "--case" | "--output" | "--left" | "--right"
            ),
            "未知选项"
        );
        ensure!(
            options.insert(pair[0].as_str(), pair[1].as_str()).is_none(),
            "选项不能重复"
        );
    }
    let output = Path::new(options.get("--output").context("缺少 --output")?);
    if options.contains_key("--left") || options.contains_key("--right") {
        ensure!(
            !["--data", "--ticket", "--dataset", "--case"]
                .iter()
                .any(|key| options.contains_key(key)),
            "比较模式不能混用工单选项"
        );
        let left: Report = read(options.get("--left").context("缺少 --left")?)?;
        let right: Report = read(options.get("--right").context("缺少 --right")?)?;
        evaluation::write_new(output, &evaluation::compare(&left, &right)?)?;
        println!("已生成本地比较报告。");
    } else {
        let data = options
            .get("--data")
            .map(PathBuf::from)
            .unwrap_or_else(sa_store::default_data_dir);
        let ticket = options
            .get("--ticket")
            .context("缺少 --ticket")?
            .parse::<i64>()
            .context("工单编号必须是正整数")?;
        let dataset: Option<Dataset> = options
            .get("--dataset")
            .map(|path| read(path))
            .transpose()?;
        let selection = match (dataset.as_ref(), options.get("--case")) {
            (Some(dataset), Some(case)) => Some((dataset, *case)),
            (None, None) => None,
            _ => bail!("--dataset 和 --case 必须同时提供"),
        };
        let report = evaluation::evaluate(&data, ticket, selection)?;
        evaluation::write_new(output, &report)?;
        println!(
            "已生成本地评测报告：{} 个任务，{} 次恢复；金额未知。",
            report.jobs.len(),
            report.recoveries
        );
    }
    Ok(())
}
