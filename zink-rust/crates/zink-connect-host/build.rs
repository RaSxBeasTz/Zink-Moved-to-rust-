fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "Zink")
            .set("FileDescription", "Zink Connect")
            .set("InternalName", "Zink Connect")
            .set("OriginalFilename", "zink-connect.exe")
            .set("CompanyName", "Zink")
            .compile()
            .expect("compile Zink Connect Windows resources");
    }
}
