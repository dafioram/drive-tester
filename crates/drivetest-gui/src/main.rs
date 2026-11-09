//! Drive Test desktop GUI.
//!
//! `drivetest-gui --demo` runs against four simulated drives (no admin rights
//! or hardware needed). On non-Windows systems the demo is always used.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod view;

use std::sync::Arc;

use drivetest_core::demo::DemoPlatform;
use drivetest_core::platform::Platform;
use eframe::egui;

fn choose_platform() -> (Arc<dyn Platform>, bool) {
    let demo_flag = std::env::args().any(|a| a == "--demo");
    #[cfg(windows)]
    if !demo_flag {
        return (Arc::new(drivetest_win::WinPlatform::new()), false);
    }
    let _ = demo_flag;
    (Arc::new(DemoPlatform::new()), true)
}

fn main() -> eframe::Result {
    let (platform, demo) = choose_platform();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(if demo { "Drive Test (demo drives)" } else { "Drive Test" })
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native("Drive Test", options, Box::new(move |_cc| Ok(Box::new(app::DriveTestApp::new(platform, demo)))))
}
