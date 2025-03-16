use std::env;
use std::path::PathBuf;

fn main() {
    let base = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.2.sdk";

    let private = format!("{base}/System/Library/PrivateFrameworks");
    println!("cargo:rustc-link-search=framework={private}");

    let hit = format!("{base}/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks");
    println!("cargo:rustc-link-search=framework={hit}");

    let zigout = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("zig-out/lib");
    let libprivate = zigout.to_string_lossy();
    println!("cargo:rustc-link-search={libprivate}");
    println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{libprivate}");
}
