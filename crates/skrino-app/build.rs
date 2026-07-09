//! Windows build script: embeds `assets/skrino.ico` as the exe's icon resource.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/skrino.ico");
        if let Err(e) = res.compile() {
            // Don't fail local non-Windows-toolchain builds over a cosmetic
            // resource; but do make it visible.
            println!("cargo:warning=failed to embed exe icon: {e}");
        }
    }
}
