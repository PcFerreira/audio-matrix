#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod background;
mod client;
mod dialogs;
mod eq_presets;
mod ipc;
mod preset;
mod process;
mod protocol;
mod tray;
mod ui_state;
mod workflow;

fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args().collect();
    let instance = args
        .windows(2)
        .find(|pair| pair[0] == "--instance")
        .map_or("default", |pair| pair[1].as_str());
    let test_mode = args.iter().any(|arg| arg == "--test-engine");
    if let Err(error) = ipc::validate_instance(instance) {
        eprintln!("{error}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--engine") {
        #[cfg(windows)]
        if let Err(error) = background::run(instance, test_mode) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--engine-request") {
        use std::io::Read;
        let request = std::io::read_to_string(std::io::stdin().take(ipc::MAX_FRAME as u64 + 1))
            .ok()
            .filter(|text| text.len() <= ipc::MAX_FRAME)
            .and_then(|text| serde_json::from_str::<protocol::Request>(&text).ok());
        let result = request
            .ok_or_else(|| "Invalid engine request JSON".to_string())
            .and_then(|request| process::rpc(instance, request));
        match result {
            Ok(reply) => println!("{}", serde_json::to_string(&reply).unwrap()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
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
    let preview = args.iter().any(|arg| arg.starts_with("--preview-"));
    #[cfg(windows)]
    let _ui_guard = if preview {
        None
    } else {
        match ipc::Instance::new(instance).and_then(|identity| identity.guard("console")) {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                let _ = process::rpc(instance, protocol::Request::Show);
                return Ok(());
            }
            Err(error) => {
                eprintln!("Single-instance protection failed: {error}");
                return Ok(());
            }
        }
    };
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
