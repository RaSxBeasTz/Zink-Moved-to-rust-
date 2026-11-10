fn main() {
    let build_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    println!("cargo:rustc-env=ZINK_BUILD_TIMESTAMP={build_timestamp}");
    slint_build::compile("ui/app-window.slint").expect("compile Zink Slint UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "Zink")
            .set("FileDescription", "Zink")
            .set("InternalName", "Zink")
            .set("OriginalFilename", "zink-app.exe")
            .set("CompanyName", "Zink")
            .compile()
            .expect("compile Zink Windows resources");
    }
}
