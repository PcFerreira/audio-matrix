#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod dialogs;
mod eq_presets;
mod preset;
mod tray;
mod ui_state;
mod workflow;

fn main() -> eframe::Result {
    if std::env::args().any(|arg| arg == "--list-devices") {
        match audio::enumerate_devices() {
            Ok(devices) => {
                for device in devices {
                    println!(
                        "{:?} | {} | {} ch | {} Hz | {}",
                        device.kind, device.name, device.channels, device.sample_rate, device.id
                    );
                }
            }
            Err(error) => {
                eprintln!("Device enumeration failed: {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Audio Matrix",
        options,
        Box::new(|cc| Ok(Box::new(app::MatrixApp::new(cc)))),
    )
}
