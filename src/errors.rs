use log::debug;
use std::{any::TypeId, fmt::Display};

/// A type alias for `std::result::Result` with a custom `Error` type.
pub type Result<T> = std::result::Result<T, Error>;

/// Represents the various types of errors that can occur within the application.
#[derive(Debug)]
pub enum Error {
    /// Indicates an invalid window operation or state.
    InvalidWindow,
    /// Indicates an issue with the application's configuration, with a descriptive message.
    InvalidConfig(String),
    /// Indicates an error during the watching or processing of configuration file changes.
    ConfigurationWatcher(String),
    /// Indicates that a requested item (e.g., a window or space) was not found, with a descriptive message.
    NotFound(String),
    /// Indicates a permission error.
    PermissionDenied(String),
    /// Indicates a problem with input.
    InvalidInput(String),
    /// Represents an I/O error, typically from `std::io::Error`.
    IO(String),
    /// A generic error with a descriptive message.
    Generic(String),
}

impl Error {
    /// Creates a new `InvalidWindow` error with a debug message.
    ///
    /// # Arguments
    ///
    /// * `message` - A string slice providing additional debug information.
    ///
    /// # Returns
    ///
    /// An `Error::InvalidWindow` instance.
    pub fn invalid_window(message: &str) -> Self {
        debug!("{message}");
        Error::InvalidWindow
    }
}

impl Display for Error {
    /// Formats the `Error` for display, providing a user-friendly error message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Error::InvalidWindow => "Invalid window".to_string(),
            Error::InvalidConfig(msg) => format!("Invalid configuration: {msg}"),
            Error::ConfigurationWatcher(msg) => format!("Watching config file: {msg}"),
            Error::NotFound(msg) => format!("Not found: {msg}"),
            Error::PermissionDenied(msg) => format!("Permission denied: {msg}"),
            Error::InvalidInput(msg) => format!("Invalid input: {msg}"),
            Error::IO(msg) => format!("IO error: {msg}"),
            Error::Generic(msg) => format!("Generic error: {msg}"),
        };
        write!(f, "{msg}")
    }
}

impl<T: std::error::Error + Display + 'static> From<T> for Error {
    fn from(err: T) -> Self {
        if TypeId::of::<T>() == TypeId::of::<std::io::Error>() {
            Error::IO(format!("{err}"))
        } else if TypeId::of::<T>() == TypeId::of::<notify::Error>() {
            Error::ConfigurationWatcher(format!("{err}"))
        } else {
            Error::Generic(format!("{err}"))
        }
    }
}
