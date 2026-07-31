//! A loadable Lua module leaves the `lua_*` symbols undefined at link time;
//! they are resolved from the host interpreter when `require` loads the module.
//!
//! These are `cdylib-link-arg`s so they apply to this crate's module only, and
//! not to anything else built in the same workspace.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
