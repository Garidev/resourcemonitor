//! Embeds a manifest enabling Common Controls v6 (cue banners, themed
//! controls). Skipped silently if windres is unavailable.

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="app.resourcemonitor" type="win32"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{}/app.manifest", out), MANIFEST).unwrap();
    let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/app.ico");
    let mut rc = String::from("1 24 \"app.manifest\"\n");
    if ico.exists() {
        std::fs::copy(&ico, format!("{}/app.ico", out)).unwrap();
        rc.push_str("1 ICON \"app.ico\"\n");
        println!("cargo:rerun-if-changed=assets/app.ico");
    }
    std::fs::write(format!("{}/res.rc", out), rc).unwrap();
    let windres = if target.contains("gnu") && !cfg!(windows) {
        "x86_64-w64-mingw32-windres"
    } else {
        "windres"
    };
    let obj = format!("{}/res.o", out);
    let status = std::process::Command::new(windres)
        .args(["res.rc", "-O", "coff", "-o"])
        .arg(&obj)
        .current_dir(&out)
        .status();
    if matches!(status, Ok(s) if s.success()) {
        println!("cargo:rustc-link-arg-bins={}", obj);
    }
}
