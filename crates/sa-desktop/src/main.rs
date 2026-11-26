//! Super Agent desktop: the workbench and scheduler for the ticket journal.
//! Falls back to a read-only observer when another workbench owns the journal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod composer;
mod control;
mod conversation;
mod data;
mod demo;
mod delivery_ui;
mod efficiency;
#[cfg(test)]
mod efficiency_tests;
mod extensions;
mod feed;
mod files;
mod host;
mod inspector;
mod knowledge;
mod labels;
mod markdown;
mod models;
mod notifications;
mod notify_ui;
#[cfg(test)]
mod notify_ui_tests;
mod ops;
mod palette;
mod providers;
mod recovery_ui;
#[cfg(test)]
mod recovery_ui_tests;
mod push;
mod review;
mod sidebar;
mod t3_import_ui;
#[cfg(test)]
mod t3_import_ui_tests;
mod theme;
mod usage;
#[cfg(test)]
mod ui_tests;
mod widgets;
mod worker;
mod workflow;
#[cfg(test)]
mod workflow_tests;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

pub(crate) const DEFAULT_WINDOW: (f32, f32) = (1440.0, 900.0);
pub(crate) const MIN_WINDOW: (f32, f32) = (880.0, 600.0);

/// Drop a corrupted or collapsed eframe `"window"` entry from `app.ron` before startup
/// so Windows minimization/off-screen saves cannot reopen the workbench at postage-stamp size.
fn strip_undersized_window(ron: &str, min_w: f32, min_h: f32) -> Option<String> {
    let mut dropped = false;
    let mut out = String::with_capacity(ron.len());
    for line in ron.split_inclusive('\n') {
        if line.trim_start().starts_with("\"window\":") {
            let undersized = match parse_inner_size_points(line) {
                Some((w, h)) => !w.is_finite() || !h.is_finite() || w < min_w || h < min_h,
                None => false,
            };
            if undersized {
                dropped = true;
                continue;
            }
        }
        out.push_str(line);
    }
    dropped.then_some(out)
}

fn parse_inner_size_points(line: &str) -> Option<(f32, f32)> {
    let rest = line.split("inner_size_points:Some((").nth(1)?;
    let pair = rest.split("))").next()?;
    let x = pair.split("x:").nth(1)?.split(',').next()?.trim().parse::<f32>().ok()?;
    let y = pair.split("y:").nth(1)?.split([',', ')']).next()?.trim().parse::<f32>().ok()?;
    Some((x, y))
}

fn sanitize_persisted_window(app_id: &str) {
    let Some(dir) = eframe::storage_dir(app_id) else {
        return;
    };
    let path = dir.join("app.ron");
    let Ok(ron) = std::fs::read_to_string(&path) else {
        return;
    };
    let Some(cleaned) = strip_undersized_window(&ron, MIN_WINDOW.0, MIN_WINDOW.1) else {
        return;
    };
    let tmp = dir.join("app.ron.tmp");
    if std::fs::write(&tmp, cleaned).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Window and taskbar icon (the shortcut uses the same `.ico`).
fn icon() -> Option<egui::IconData> {
    let ico = include_bytes!("../../../assets/super-agent.ico");
    let img = image::load_from_memory_with_format(ico, image::ImageFormat::Ico).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn start_scheduler(
    opts: &cli::Options,
    start: impl FnOnce() -> anyhow::Result<host::Host>,
) -> anyhow::Result<host::Host> {
    if opts.demo {
        Err(anyhow::anyhow!("演示数据不调度"))
    } else if opts.observe {
        Err(anyhow::anyhow!("以 --observe 启动，不调度"))
    } else if opts.screenshot.is_some() && !opts.schedule {
        Err(anyhow::anyhow!("截图模式不调度"))
    } else {
        start()
    }
}

fn window_persists(opts: &cli::Options, has_host: bool) -> bool {
    has_host && !opts.observe && opts.screenshot.is_none()
}

fn native_options(
    opts: &cli::Options,
    has_host: bool,
    viewport: egui::ViewportBuilder,
) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport,
        centered: true,
        persist_window: window_persists(opts, has_host),
        // Keep screenshots independent of the user's saved layout.
        persistence_path: opts.screenshot.is_some()
            .then(|| std::env::temp_dir().join("super-agent-screenshot-state")),
        ..Default::default()
    }
}

fn create_app_with_host(
    cc: &eframe::CreationContext<'_>,
    opts: cli::Options,
    dir: PathBuf,
    opener: data::Opener,
    shot_result: Arc<Mutex<Option<Result<(), String>>>>,
    host: anyhow::Result<host::Host>,
) -> app::App {
    theme::install_fonts(&cc.egui_ctx);
    let host_error = host.as_ref().err();
    let observer = !opts.demo
        && (opts.observe
            || host_error.is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.to_string() == sa_engine::owner::BUSY)
            }));
    let panel = if observer {
        None
    } else {
        host_error
            .and_then(|error| error.downcast_ref::<sa_store::journal::JournalError>())
            .map(|error| recovery_ui::RecoveryPanel::from_journal(error, &dir, false))
    };
    let worker = worker::spawn(opener, cc.egui_ctx.clone());
    let mut app = app::App::new(
        cc,
        opts,
        worker,
        shot_result,
        host.map_err(|error| format!("{error:#}")),
    );
    app.recovery = panel;
    if observer {
        app.inspect_recovery(&cc.egui_ctx, dir);
    }
    app
}

#[cfg(test)]
fn create_app(
    cc: &eframe::CreationContext<'_>,
    opts: cli::Options,
    dir: PathBuf,
    opener: data::Opener,
    shot_result: Arc<Mutex<Option<Result<(), String>>>>,
    start_host: impl FnOnce(PathBuf, egui::Context) -> anyhow::Result<host::Host>,
) -> app::App {
    let host = start_scheduler(&opts, || start_host(dir.clone(), cc.egui_ctx.clone()));
    create_app_with_host(cc, opts, dir, opener, shot_result, host)
}

fn main() {
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };
    let (w, h) = opts.size.unwrap_or(DEFAULT_WINDOW);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Super Agent")
        .with_inner_size([w, h])
        .with_min_inner_size([MIN_WINDOW.0, MIN_WINDOW.1])
        .with_app_id("super-agent");
    if let Some(i) = icon() {
        viewport = viewport.with_icon(Arc::new(i));
    }
    if opts.screenshot.is_some() {
        // Exact requested size; the window is not user-resizable during capture.
        viewport = viewport
            .with_min_inner_size([320.0, 240.0])
            .with_resizable(false);
    }
    let dir = opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
    // eframe fixes its window-persistence policy before constructing the app.
    // Acquire the actual scheduler lock first, including automatic observers.
    let repaint = Arc::new(OnceLock::<egui::Context>::new());
    let wake = repaint.clone();
    let host = start_scheduler(&opts, || {
        host::Host::start(dir.clone(), move || {
            if let Some(ctx) = wake.get() {
                ctx.request_repaint();
            }
        })
    });
    let native = native_options(&opts, host.is_ok(), viewport);
    if native.persist_window {
        sanitize_persisted_window("super-agent");
    }

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
            let _ = repaint.set(cc.egui_ctx.clone());
            Ok(Box::new(create_app_with_host(
                cc,
                opts,
                dir,
                opener,
                result,
                host,
            )))
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

#[cfg(test)]
mod window_guard_tests {
    use super::{MIN_WINDOW, strip_undersized_window};

    #[test]
    fn drops_collapsed_height_window_entry_and_keeps_other_settings() {
        let ron = "{\n    \"window\": \"(inner_position_pixels:Some((x:209.0,y:798.0)),outer_position_pixels:Some((x:200.0,y:760.0)),fullscreen:false,maximized:false,inner_size_points:Some((x:1712.0,y:64.0)))\",\n    \"dark\": \"0\",\n    \"project\": \"E:\\\\work\",\n}\n";
        let cleaned = strip_undersized_window(ron, MIN_WINDOW.0, MIN_WINDOW.1)
            .expect("undersized window entry should be dropped");
        assert!(!cleaned.contains("\"window\":"));
        assert!(cleaned.contains("\"dark\": \"0\""));
        assert!(cleaned.contains("\"project\": \"E:\\\\work\""));
    }

    #[test]
    fn drops_collapsed_width_window_entry() {
        let ron = "{\n    \"window\": \"(fullscreen:false,inner_size_points:Some((x:120.0,y:720.0)))\",\n    \"dark\": \"1\",\n}\n";
        let cleaned = strip_undersized_window(ron, MIN_WINDOW.0, MIN_WINDOW.1)
            .expect("narrow window entry should be dropped");
        assert!(!cleaned.contains("\"window\":"));
        assert!(cleaned.contains("\"dark\": \"1\""));
    }

    #[test]
    fn preserves_valid_window_sizes_including_exact_minimum() {
        let normal = "{\n    \"window\": \"(fullscreen:false,inner_size_points:Some((x:1440.0,y:900.0)))\",\n}\n";
        assert!(strip_undersized_window(normal, MIN_WINDOW.0, MIN_WINDOW.1).is_none());
        let exact_min = "{\n    \"window\": \"(fullscreen:false,inner_size_points:Some((x:880.0,y:600.0)))\",\n}\n";
        assert!(strip_undersized_window(exact_min, MIN_WINDOW.0, MIN_WINDOW.1).is_none());
    }
}
