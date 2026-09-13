fn main() {
    // Embed the Windows .exe icon. No-op when cross-compiled to a non-Windows
    // target. The rest of the crate is Windows-only anyway, but guarding here
    // keeps `cargo check --target=...` clean if someone tries it.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("logo.ico");
        res.compile().expect("failed to compile Windows resource");
    }
}
