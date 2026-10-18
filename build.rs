fn main() {
    println!("cargo:rerun-if-changed=assets/icons/app-icon.ico");

    // gpui loads the window and taskbar icon from resource ID 1 of the exe.
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let app_name = std::env::var("CARGO_PKG_NAME").expect("cargo sets CARGO_PKG_NAME");
        winresource::WindowsResource::new()
            .set_icon_with_id("assets/icons/app-icon.ico", "1")
            .set("ProductName", &app_name)
            .set("FileDescription", &app_name)
            .compile()
            .expect("failed to embed Windows resources");
    }
}
