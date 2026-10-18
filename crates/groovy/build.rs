use std::path::PathBuf;

fn main() {
    // Build scripts run from the package directory, so resolve assets from the manifest.
    let icon =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"))
            .join("../../assets/icons/app-icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    // gpui loads the window and taskbar icon from resource ID 1 of the exe.
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let app_name = std::env::var("CARGO_PKG_NAME").expect("cargo sets CARGO_PKG_NAME");
        winresource::WindowsResource::new()
            .set_icon_with_id(icon.to_str().expect("icon path is valid UTF-8"), "1")
            .set("ProductName", &app_name)
            .set("FileDescription", &app_name)
            .compile()
            .expect("failed to embed Windows resources");
    }
}
