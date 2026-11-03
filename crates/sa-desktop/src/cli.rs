//! Command line: `super-agent [--data <dir>] [--ticket <id>] [--demo] [--observe] [--schedule] [--dark]
//! [--inspector [tab]] [--dialog models|providers|rotation] [--screenshot <out.png> [--size WxH]]`.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub const USAGE: &str = "用法: super-agent [--data <目录>] [--ticket <编号>] [--demo] [--observe] [--schedule] [--dark] \
[--inspector [collab|native|plan|approvals|diff]] [--dialog models|providers|rotation] [--at-top] [--expand-all] [--screenshot <输出.png> [--size 宽x高]]";

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub data: Option<PathBuf>,
    pub ticket: Option<i64>,
    pub demo: bool,
    /// Never claim the journal; only observe it.
    pub observe: bool,
    /// Schedule even while taking a screenshot.
    pub schedule: bool,
    pub dark: bool,
    /// `Some(None)` opens the inspector on its current tab.
    pub inspector: Option<Option<String>>,
    pub screenshot: Option<PathBuf>,
    pub size: Option<(f32, f32)>,
    /// Keep the conversation scrolled to the top (screenshots of long feeds).
    pub at_top: bool,
    /// Expand every progress group and job card (screenshot review aid).
    pub expand_all: bool,
    /// Open a dialog at start (`models`; screenshot review aid).
    pub dialog: Option<String>,
}

fn parse_size(s: &str) -> Result<(f32, f32)> {
    let (w, h) = s
        .split_once(['x', 'X', '*'])
        .with_context(|| format!("尺寸格式应为 宽x高：{s}"))?;
    let w: f32 = w.trim().parse().with_context(|| format!("无效宽度：{w}"))?;
    let h: f32 = h.trim().parse().with_context(|| format!("无效高度：{h}"))?;
    if !(320.0..=8192.0).contains(&w) || !(240.0..=8192.0).contains(&h) {
        bail!("尺寸超出范围：{s}");
    }
    Ok((w, h))
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut o = Options::default();
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} 缺少参数值"));
        match arg.as_str() {
            "--data" => o.data = Some(PathBuf::from(value("--data")?)),
            "--ticket" => {
                let v = value("--ticket")?;
                let id: i64 = v.parse().with_context(|| format!("无效工单编号：{v}"))?;
                if id <= 0 {
                    bail!("无效工单编号：{v}");
                }
                o.ticket = Some(id);
            }
            "--demo" => o.demo = true,
            "--observe" => o.observe = true,
            "--schedule" => o.schedule = true,
            "--dark" => o.dark = true,
            "--at-top" => o.at_top = true,
            "--expand-all" => o.expand_all = true,
            "--dialog" => match value("--dialog")?.as_str() {
                d @ ("models" | "providers" | "rotation") => o.dialog = Some(d.into()),
                other => bail!("未知对话框：{other}"),
            },
            "--screenshot" => o.screenshot = Some(PathBuf::from(value("--screenshot")?)),
            "--size" => o.size = Some(parse_size(&value("--size")?)?),
            "--inspector" => {
                let tab = match it.peek() {
                    Some(next) if !next.starts_with("--") => it.next(),
                    _ => None,
                };
                o.inspector = Some(tab);
            }
            "-h" | "--help" => bail!("{USAGE}"),
            other => bail!("未知参数：{other}\n{USAGE}"),
        }
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Options> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_all_flags() {
        let o = p(&[
            "--demo",
            "--dark",
            "--ticket",
            "7",
            "--inspector",
            "plan",
            "--screenshot",
            "a.png",
            "--size",
            "1000x760",
        ])
        .unwrap();
        assert!(o.demo && o.dark);
        assert_eq!(o.ticket, Some(7));
        assert_eq!(o.inspector, Some(Some("plan".into())));
        assert_eq!(o.size, Some((1000.0, 760.0)));
        assert_eq!(o.screenshot, Some(PathBuf::from("a.png")));
    }

    #[test]
    fn inspector_without_tab() {
        let o = p(&["--inspector", "--demo"]).unwrap();
        assert_eq!(o.inspector, Some(None));
        assert!(o.demo);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--size", "abc"]).is_err());
        assert!(p(&["--ticket", "-1"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["--data"]).is_err());
    }
}
