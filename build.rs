fn main() {
    println!("cargo:rerun-if-changed=assets/optim-bar.ico");
    // Full version-info block + manifest: unsigned binaries with sparse
    // metadata score worse with Defender/SmartScreen reputation heuristics.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let parts: Vec<u64> = version
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let vnum = (parts[0] << 48) | (parts[1] << 32) | (parts[2] << 16);
    winres::WindowsResource::new()
        .set_icon_with_id("assets/optim-bar.ico", "app_icon")
        .set("FileDescription", "optim-bar — minimal native status bar")
        .set("ProductName", "optim-bar")
        .set("CompanyName", "brian-gee (github.com/brian-gee/optim-bar)")
        .set("LegalCopyright", "MIT License — github.com/brian-gee/optim-bar")
        .set("OriginalFilename", "optim-bar.exe")
        .set("InternalName", "optim-bar")
        .set_version_info(winres::VersionInfo::FILEVERSION, vnum)
        .set_version_info(winres::VersionInfo::PRODUCTVERSION, vnum)
        .set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 / 11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>"#,
        )
        .compile()
        .expect("failed to embed resources");
}
