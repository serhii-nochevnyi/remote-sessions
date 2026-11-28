fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        let manifest = tauri_build::AppManifest::new().commands(&[
            "w2_begin_attempt",
            "w2_append_input",
            "w2_read_attempt",
        ]);
        tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
            .expect("failed to build the iOS recorder app");
    }
}
