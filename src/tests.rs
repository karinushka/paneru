use std::sync::{Arc, RwLock};

use bevy::prelude::*;

mod display;
mod harness;
mod interaction;
mod mocks;
mod tiling;

pub(crate) use harness::*;
pub(crate) use mocks::*;

pub(crate) const TEST_PROCESS_ID: i32 = 1;
pub(crate) const TEST_DISPLAY_ID: u32 = 1;
pub(crate) const TEST_WORKSPACE_ID: u64 = 2;
pub(crate) const TEST_DISPLAY_WIDTH: i32 = 1024;
pub(crate) const TEST_DISPLAY_HEIGHT: i32 = 768;

pub(crate) const TEST_MENUBAR_HEIGHT: i32 = 20;
pub(crate) const TEST_WINDOW_WIDTH: i32 = 400;
pub(crate) const TEST_WINDOW_HEIGHT: i32 = 1000;

/// Type alias for a shared, thread-safe queue of `Event`s, used for simulating internal events in tests.
pub(crate) type EventQueue = Arc<RwLock<Vec<Event>>>;

pub(crate) type TestWindowSpawner = Box<dyn Fn(WorkspaceId) -> Vec<Window> + Send + Sync + 'static>;

use crate::events::Event;
use crate::manager::Window;
use crate::platform::WorkspaceId;
