fn main() {
    println!("cargo:rerun-if-changed=assets/optim-bar.ico");
    winres::WindowsResource::new()
        .set_icon_with_id("assets/optim-bar.ico", "app_icon")
        .compile()
        .expect("failed to embed icon resource");
}
