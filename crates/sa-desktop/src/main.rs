//! Super Agent desktop: the workbench and scheduler for the ticket journal.
//! Falls back to a read-only observer when another workbench owns the journal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod composer;
mod conversation;
mod data;
mod demo;
mod feed;
mod host;
mod inspector;
mod labels;
mod markdown;
mod sidebar;
mod theme;
#[cfg(test)]
mod ui_tests;
mod widgets;
mod worker;

use std::sync::{Arc, Mutex};

fn main() {
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };
    let (w, h) = opts.size.unwrap_or((1440.0, 900.0));
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Super Agent")
        .with_inner_size([w, h])
        .with_min_inner_size([880.0, 600.0])
        .with_app_id("super-agent");
    if opts.screenshot.is_some() {
        // Exact requested size; the window is not user-resizable during capture.
        viewport = viewport
            .with_min_inner_size([320.0, 240.0])
            .with_resizable(false);
    }
    let mut native = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    if opts.screenshot.is_some() {
        // Keep screenshots independent of the user's saved layout.
        native.persistence_path = Some(std::env::temp_dir().join("super-agent-screenshot-state"));
        native.persist_window = false;
    }

    let dir = opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
    // Demo data never schedules; screenshots only with `--schedule` (so a regression
    // capture never claims the live journal); `--observe` keeps the window read-only.
    let schedule = !opts.demo && !opts.observe && (opts.screenshot.is_none() || opts.schedule);
    let opener: data::Opener = if opts.demo {
        Box::new(|| Ok(Box::new(demo::DemoSource::new()) as Box<dyn data::DataSource>))
    } else {
        data::StoreSource::opener(dir.clone())
    };
    let shot_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
    let result = Arc::clone(&shot_result);
    let wants_shot = opts.screenshot.is_some();
    let run = eframe::run_native(
        "super-agent",
        native,
        Box::new(move |cc| {
            theme::install_fonts(&cc.egui_ctx);
            // The scheduler creates the journal, so start it before the reader.
            let host = if schedule {
                let ctx = cc.egui_ctx.clone();
                host::Host::start(dir, move || ctx.request_repaint()).map_err(|e| format!("{e:#}"))
            } else if opts.demo {
                Err("演示数据不调度".into())
            } else if opts.observe {
                Err("以 --observe 启动，不调度".into())
            } else {
                Err("截图模式不调度".into())
            };
            let worker = worker::spawn(opener, cc.egui_ctx.clone());
            Ok(Box::new(app::App::new(cc, opts, worker, result, host)))
        }),
    );
    if let Err(e) = run {
        eprintln!("窗口启动失败：{e}");
        std::process::exit(1);
    }
    if wants_shot {
        match shot_result.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
            None => {
                eprintln!("窗口在截图完成前关闭");
                std::process::exit(1);
            }
        }
    }
}
