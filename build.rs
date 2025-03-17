use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let base = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.2.sdk";

    let private = format!("{base}/System/Library/PrivateFrameworks");
    println!("cargo:rustc-link-search=framework={private}");

    let hit = format!("{base}/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks");
    println!("cargo:rustc-link-search=framework={hit}");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    let zig_src = manifest_dir.join("src/root.zig");
    let zig_out = manifest_dir.join("zig-out/lib");

    println!("cargo:rerun-if-changed={}", zig_src.to_string_lossy());
    let status = Command::new("zig")
        .args(["build"])
        .status()
        .expect("Failed to execute zig build");
    if !status.success() {
        panic!("zig build failed");
    }

    let zig_out = zig_out.to_string_lossy();
    println!("cargo:rustc-link-search={zig_out}");
    println!("cargo:rustc-link-lib=static=private");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{zig_out}");
}
