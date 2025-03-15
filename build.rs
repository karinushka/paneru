fn main() {
    let base = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.2.sdk";

    let private = format!("{base}/System/Library/PrivateFrameworks");
    println!("cargo:rustc-link-search=framework={private}");

    let hit = format!("{base}/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks");
    println!("cargo:rustc-link-search=framework={hit}");

    let zigout = "zig-out/lib";
    println!("cargo:rustc-link-search={zigout}");
    println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{zigout}");
}
