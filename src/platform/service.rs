use std::{
    env, fs,
    io::{Error, ErrorKind, Result, Write},
    path::{Path, PathBuf},
};

use log::{info, warn};

use crate::util::exe_path;

/// The bundle identifier for the `paneru` service.
pub const ID: &str = "com.github.karinushka.paneru";

/// `Service` manages the installation, uninstallation, starting, and stopping of the `paneru` application as a launchd service.
/// It encapsulates the `launchctl::Service` and the path to the executable.
#[derive(Debug)]
pub struct Service {
    /// The underlying `launchctl::Service` instance.
    pub raw: launchctl::Service,
    /// The absolute path to the `paneru` executable.
    pub bin_path: PathBuf,
}

impl Service {
    /// Creates a new `Service` instance.
    /// It determines the executable path and constructs the `launchctl::Service` with appropriate settings.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the service (e.g., "com.github.karinushka.paneru").
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the service is created successfully, otherwise `Err(Error)` if the executable path or home directory cannot be found.
    pub fn try_new(name: &str) -> Result<Self> {
        Ok(Self {
            bin_path: exe_path().ok_or(Error::new(
                ErrorKind::NotFound,
                "Cannot find current executable path.",
            ))?,
            raw: launchctl::Service::builder()
                .name(name)
                .uid(unsafe { libc::getuid() }.to_string())
                .plist_path(format!(
                    "{home}/Library/LaunchAgents/{name}.plist",
                    home = env::home_dir()
                        .ok_or(Error::new(
                            ErrorKind::NotFound,
                            "Cannot find home directory.",
                        ))?
                        .display()
                ))
                .build(),
        })
    }

    /// Returns the path to the launchd plist file for this service.
    #[must_use]
    pub fn plist_path(&self) -> &Path {
        Path::new(&self.raw.plist_path)
    }

    /// Checks if the service is currently installed (i.e., its plist file exists).
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.plist_path().is_file()
    }

    /// Installs the service as a launch agent by writing its plist file.
    /// If the service is already installed, a warning is logged, and installation is skipped.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is installed successfully or already exists, otherwise `Err(Error)` if a file system error occurs.
    pub fn install(&self) -> Result<()> {
        let plist_path = self.plist_path();
        if self.is_installed() {
            warn!(
                "existing launch agent detected at `{}`, skipping installation",
                plist_path.display()
            );
            return Ok(());
        }

        let mut plist = fs::File::create(plist_path)?;
        plist.write_all(self.launchd_plist().as_bytes())?;
        info!("installed launch agent to `{}`", plist_path.display());
        Ok(())
    }

    /// Uninstalls the service by removing its plist file.
    /// If the service is not installed, a warning is logged, and uninstallation is skipped.
    /// It also attempts to stop the service before removing the file.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is uninstalled successfully or not found, otherwise `Err(Error)` if a file system error occurs.
    pub fn uninstall(&self) -> Result<()> {
        let plist_path = self.plist_path();
        if !self.is_installed() {
            warn!(
                "no launch agent detected at `{}`, skipping uninstallation",
                plist_path.display(),
            );
            return Ok(());
        }

        if let Err(e) = self.stop() {
            warn!("failed to stop service: {e:?}");
        }

        fs::remove_file(plist_path)?;
        info!(
            "removed existing launch agent at `{}`",
            plist_path.display()
        );
        Ok(())
    }

    /// Reinstalls the service by first uninstalling it and then installing it again.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is reinstalled successfully, otherwise `Err(Error)` from underlying install/uninstall operations.
    pub fn reinstall(&self) -> Result<()> {
        self.uninstall()?;
        self.install()
    }

    /// Starts the service using `launchctl`.
    /// If the service is not installed, it will be installed first.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service starts successfully, otherwise `Err(Error)` from `launchctl`.
    pub fn start(&self) -> Result<()> {
        if !self.is_installed() {
            self.install()?;
        }
        info!("starting service...");
        self.raw.start()?;
        info!("service started");
        Ok(())
    }

    /// Stops the service using `launchctl`.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service stops successfully, otherwise `Err(Error)` from `launchctl`.
    pub fn stop(&self) -> Result<()> {
        info!("stopping service...");
        self.raw.stop()?;
        info!("service stopped");
        Ok(())
    }

    /// Restarts the service by first stopping it and then starting it again.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service restarts successfully, otherwise `Err(Error)` from underlying stop/start operations.
    pub fn restart(&self) -> Result<()> {
        self.stop()?;
        self.start()
    }

    /// Generates the content of the launchd plist file for this service.
    /// This string is formatted with the service name, executable path, and log paths.
    #[must_use]
    pub fn launchd_plist(&self) -> String {
        format!(
            include_str!("../../assets/launchd.plist"),
            name = self.raw.name,
            bin_path = self.bin_path.display(),
            out_log_path = self.raw.out_log_path,
            error_log_path = self.raw.error_log_path,
        )
    }
}
