//! Super Agent desktop: a read-only observer of the ticket journal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod conversation;
mod data;
mod demo;
mod feed;
mod inspector;
mod labels;
mod markdown;
mod sidebar;
mod theme;
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

    let opener: data::Opener = if opts.demo {
        Box::new(|| Ok(Box::new(demo::DemoSource::new()) as Box<dyn data::DataSource>))
    } else {
        let dir = opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        data::StoreSource::opener(dir)
    };
    let shot_result: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
    let result = Arc::clone(&shot_result);
    let wants_shot = opts.screenshot.is_some();
    let run = eframe::run_native(
        "super-agent",
        native,
        Box::new(move |cc| {
            theme::install_fonts(&cc.egui_ctx);
            let worker = worker::spawn(opener, cc.egui_ctx.clone());
            Ok(Box::new(app::App::new(cc, opts, worker, result)))
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
