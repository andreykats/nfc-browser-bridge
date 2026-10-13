//! Windows: embed the icon and version information in the .exe.

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/app.ico");
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("../../packaging/windows/app.ico")
                .set("ProductName", "Topps NFC Bridge")
                .set("FileDescription", "Topps NFC Bridge")
                .set("CompanyName", "drey.dev")
                .set("LegalCopyright", "MIT License");
            if let Err(e) = res.compile() {
                panic!("winresource: {e}");
            }
        }
    }
}
