// No cc crate/download needed. These commands compile only
// the bridge into the phone binary; they do not sign, install, or spawn a helper.
use std::{path::PathBuf, process::Command};
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let object = out.join("maps_lookup.o");
    assert!(
        Command::new("/usr/bin/xcrun")
            .args([
                "clang",
                "-fobjc-arc",
                "-fblocks",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-mmacosx-version-min=15.0",
                "-c",
                "native/maps_lookup.m",
                "-o"
            ])
            .arg(&object)
            .status()
            .expect("clang available")
            .success()
    );
    assert!(
        Command::new("/usr/bin/xcrun")
            .args(["ar", "crs"])
            .arg(out.join("libmaps_lookup.a"))
            .arg(&object)
            .status()
            .expect("ar available")
            .success()
    );
    // Rust links with -nodefaultlibs. Clang's @available lowering needs its
    // matching Darwin availability runtime even when release LTO removes other
    // Objective-C users. Resolve from the selected toolchain, never a fixed path.
    let runtime = Command::new("/usr/bin/xcrun")
        .args(["clang", "--print-file-name=libclang_rt.osx.a"])
        .output()
        .expect("Clang Darwin runtime lookup must succeed");
    assert!(
        runtime.status.success(),
        "Clang Darwin runtime lookup failed"
    );
    let runtime = String::from_utf8(runtime.stdout).expect("Clang runtime path must be UTF-8");
    let runtime = runtime.trim();
    assert!(
        !runtime.contains(['\r', '\n']),
        "Clang runtime lookup must return one path"
    );
    let runtime = PathBuf::from(runtime);
    assert!(
        runtime.is_absolute()
            && runtime.is_file()
            && runtime
                .file_name()
                .is_some_and(|name| name == "libclang_rt.osx.a"),
        "Selected toolchain is missing its Darwin runtime archive"
    );
    let runtime_dir = runtime
        .parent()
        .expect("Absolute runtime archive has a parent");
    println!("cargo:rustc-link-search=native={}", runtime_dir.display());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=maps_lookup");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
    println!("cargo:rustc-link-lib=framework=MapKit");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-changed=native/maps_lookup.m");
    println!("cargo:rerun-if-changed=native/maps_lookup.h");
}
