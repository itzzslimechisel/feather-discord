//! Feather: a lightweight, native Discord bot-account client.

mod app;
mod bench;
mod fonts;
mod imgcache;
mod rows;
mod theme;
mod ui;

use eframe::egui;
use feather_core::config::Endpoints;
use feather_core::diag;
use feather_core::engine::{self, EngineConfig, Session};
use feather_core::gateway::ConnState;
use feather_core::settings::Settings;
use std::sync::Arc;

fn data_dir() -> std::path::PathBuf {
    if let Some(d) = std::env::var_os("FEATHER_DATA_DIR") {
        return d.into();
    }
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("Feather")
}

fn icon() -> egui::IconData {
    // A simple original mark: a rounded accent square with a light diagonal feather stroke.
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 / n as f32, y as f32 / n as f32);
            let r = 0.22;
            let (cx, cy) = ((fx - 0.5).abs() - (0.5 - r), (fy - 0.5).abs() - (0.5 - r));
            let d = (cx.max(0.0).powi(2) + cy.max(0.0).powi(2)).sqrt() - r;
            if d > 0.0 {
                continue;
            }
            let stroke = ((fx + fy) - 1.0).abs() < 0.07 && fx > 0.2 && fx < 0.8;
            let barb = (((fx + fy) - 1.0 + 0.16 * ((fx - 0.5) * 6.0).sin()).abs() < 0.03) && fx > 0.3 && fx < 0.7;
            let (r, g, b) = if stroke || barb { (245, 247, 255) } else { (79, 140, 255) };
            let i = (y * n + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
}

fn main() -> eframe::Result {
    diag::init_clock();
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    diag::init_logging(Some(dir.join("feather.log")));
    diag::mark("main_start");

    let settings_path = dir.join("settings.json");
    let loaded = Settings::load(&settings_path);
    let settings = loaded.settings;
    let recovered_settings = loaded.recovered_from.is_some();
    diag::mark("settings_loaded");

    let (w, h) = settings.window_size.unwrap_or((1100.0, 720.0));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Feather")
            .with_app_id("feather")
            .with_inner_size([w, h])
            .with_min_inner_size([640.0, 420.0])
            .with_icon(Arc::new(icon())),
        renderer: eframe::Renderer::Glow,
        vsync: true,
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };

    eframe::run_native(
        "Feather",
        options,
        Box::new(move |cc| {
            diag::mark("window_created");
            let endpoints = Endpoints::from_env();
            let ctx = cc.egui_ctx.clone();
            let wake_ctx = ctx.clone();
            let engine = engine::spawn(
                EngineConfig {
                    data_dir: dir.clone(),
                    endpoints: endpoints.clone(),
                    intents: settings.intents.clone(),
                    disk_messages_per_channel: settings.disk_messages_per_channel,
                    notifications: settings.notifications,
                    notify_all_dms: settings.notify_all_dms,
                },
                Arc::new(move || wake_ctx.request_repaint()),
            );
            Ok(Box::new(ui::new_app(ctx, engine, endpoints, settings, settings_path, recovered_settings)))
        }),
    )
}

#[allow(dead_code)]
fn _types(_: Session, _: ConnState) {}
