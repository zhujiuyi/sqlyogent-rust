fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    use std::{env, path::PathBuf};

    let target = env::var("TARGET").expect("missing Cargo build target");
    if !target.ends_with("-windows-msvc") {
        return;
    }

    println!("cargo::rerun-if-changed=assets/windows.rc");
    println!("cargo::rerun-if-changed=assets/dolphin.ico");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("missing Cargo output directory"))
        .join("harbor-sql.res");
    let version_number = concat!(
        env!("CARGO_PKG_VERSION_MAJOR"),
        ",",
        env!("CARGO_PKG_VERSION_MINOR"),
        ",",
        env!("CARGO_PKG_VERSION_PATCH"),
        ",0"
    );
    let version_text = env!("CARGO_PKG_VERSION");
    let status = find_msvc_tools::find(&target, "rc.exe")
        .expect("Windows SDK resource compiler (rc.exe) is required")
        .args(["/nologo", "/c", "65001", "/fo"])
        .arg(&output)
        .arg("/d")
        .arg(format!("APP_VERSION_NUMBER={version_number}"))
        .arg("/d")
        .arg(format!(r#"APP_VERSION_TEXT="{version_text}""#))
        .arg("assets/windows.rc")
        .status()
        .expect("could not run the Windows SDK resource compiler");
    assert!(
        status.success(),
        "Windows SDK resource compiler failed: {status}"
    );

    println!("cargo::rustc-link-arg-bin=harbor-sql={}", output.display());
}
