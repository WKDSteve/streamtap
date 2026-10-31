// Only the app's own commands are exposed to the webview (see capabilities/main.json).
const COMMANDS: &[&str] = &[
    "snapshot",
    "get_settings",
    "save_settings",
    "regenerate_token",
    "start_sink",
    "stop_sink",
    "mux_capture",
    "open_result",
    "open_folder",
    "delete_capture",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri build failed");
}
