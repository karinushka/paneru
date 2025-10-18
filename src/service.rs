use std::{
    env, fs,
    io::{Error, ErrorKind, Result, Write},
    path::{Path, PathBuf},
};

use log::{info, warn};

use crate::util::exe_path;

pub const ID: &str = "com.github.karinushka.paneru";

#[derive(Debug)]
pub struct Service {
    pub raw: launchctl::Service,
    pub bin_path: PathBuf,
}

impl Service {
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

    #[must_use]
    pub fn plist_path(&self) -> &Path {
        Path::new(&self.raw.plist_path)
    }

    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.plist_path().is_file()
    }

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

    pub fn reinstall(&self) -> Result<()> {
        self.uninstall()?;
        self.install()
    }

    pub fn start(&self) -> Result<()> {
        if !self.is_installed() {
            self.install()?;
        }
        info!("starting service...");
        self.raw.start()?;
        info!("service started");
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        info!("stopping service...");
        self.raw.stop()?;
        info!("service stopped");
        Ok(())
    }

    pub fn restart(&self) -> Result<()> {
        self.stop()?;
        self.start()
    }

    #[must_use]
    pub fn launchd_plist(&self) -> String {
        format!(
            include_str!("../assets/launchd.plist"),
            name = self.raw.name,
            bin_path = self.bin_path.display(),
            out_log_path = self.raw.out_log_path,
            error_log_path = self.raw.error_log_path,
        )
    }
}
