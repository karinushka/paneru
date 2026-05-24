mod display;
mod harness;
mod interaction;
mod mocks;
mod session_restore;
mod state;
mod tabs;
mod tiling;

pub(crate) use harness::*;
pub(crate) use mocks::*;

pub(crate) const TEST_PROCESS_ID: i32 = 1;
pub(crate) const TEST_DISPLAY_ID: u32 = 1;
pub(crate) const TEST_WORKSPACE_ID: u64 = 2;
pub(crate) const TEST_DISPLAY_WIDTH: i32 = 1024;
pub(crate) const TEST_DISPLAY_HEIGHT: i32 = 768;

pub(crate) const EXT_DISPLAY_ID: u32 = 2;
pub(crate) const EXT_WORKSPACE_ID: u64 = 20;
pub(crate) const EXT_DISPLAY_WIDTH: i32 = 1920;
pub(crate) const EXT_DISPLAY_HEIGHT: i32 = 1200;

pub(crate) const TEST_MENUBAR_HEIGHT: i32 = 20;
pub(crate) const TEST_WINDOW_WIDTH: i32 = 400;
pub(crate) const TEST_WINDOW_HEIGHT: i32 = 1000;

#[allow(unused_imports)]
use crate::events::Event;
#[allow(unused_imports)]
use crate::manager::Window;
#[allow(unused_imports)]
use crate::platform::WorkspaceId;
