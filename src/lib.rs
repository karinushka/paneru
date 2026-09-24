//! The `paneru` shared types and Lua API library.
//!
//! As an `rlib`, the `paneru` daemon binary links against this library for wire
//! types, command definitions, and the shared Lua API (`paneru.window.*`,
//! `paneru.workspace.*`, `paneru.mouse.*`).
//!
//! When built with `--lib --no-default-features --features module,lua54` (or
//! another Lua ABI feature such as `luajit`, `lua53`, `lua52`, `lua55`), this
//! target also produces the loadable `cdylib` (`libpaneru.dylib` / `paneru.so`)
//! exposing `luaopen_paneru` for external Lua scripts (`require("paneru")`).

#![allow(
    clippy::needless_pass_by_value,
    reason = "mlua callback signatures are by-value by contract"
)]

pub mod types;

#[cfg(feature = "lua-base")]
pub mod lua {
    #[path = "client.rs"]
    pub mod client;
    #[path = "shared.rs"]
    pub mod shared;
}
