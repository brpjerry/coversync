fn main() {
    // The app's commands need a permission, which only the bundled pages'
    // capability grants (capabilities/main.json).
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["status", "connect"])),
    )
    .expect("tauri-build failed");
}
