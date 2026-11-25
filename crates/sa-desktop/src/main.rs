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
mod ops;
mod palette;
mod providers;
mod recovery_ui;
#[cfg(test)]
mod recovery_ui_tests;
mod review;
mod sidebar;
mod theme;
mod usage;
#[cfg(test)]
mod ui_tests;
mod widgets;
mod worker;
mod workflow;
#[cfg(test)]
mod workflow_tests;

use std::sync::{Arc, Mutex};

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
    let mut native = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    if opts.screenshot.is_some() {
        // Keep screenshots independent of the user's saved layout.
        native.persistence_path = Some(std::env::temp_dir().join("super-agent-screenshot-state"));
        native.persist_window = false;
    } else {
        sanitize_persisted_window("super-agent");
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
            // The typed startup error is kept for the recovery panel; every
            // other consumer still gets today's message.
            let mut start_error = None;
            let host = if schedule {
                let ctx = cc.egui_ctx.clone();
                host::Host::start(dir, move || ctx.request_repaint()).map_err(|e| {
                    let message = format!("{e:#}");
                    start_error = Some(e);
                    message
                })
            } else if opts.demo {
                Err("演示数据不调度".into())
            } else if opts.observe {
                Err("以 --observe 启动，不调度".into())
            } else {
                Err("截图模式不调度".into())
            };
            let worker = worker::spawn(opener, cc.egui_ctx.clone());
            let mut app = app::App::new(cc, opts, worker, result, host);
            if let Some(error) = &start_error {
                app.set_start_error(error);
            }
            Ok(Box::new(app))
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
