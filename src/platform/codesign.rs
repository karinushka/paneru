//! Signing for the binaries Paneru installs.
//!
//! An ad-hoc signature makes a binary's designated requirement its own cdhash,
//! which is what TCC stores when Accessibility is granted. Every rebuild changes
//! the cdhash and silently voids the grant. A stable certificate moves the
//! requirement onto the certificate instead.

use std::{
    env,
    io::{Error, Result},
    path::Path,
    process::Command,
};

use tracing::info;

pub const SERVICE_IDENTIFIER: &str = "com.github.karinushka.paneru";
pub const LAUNCHER_IDENTIFIER: &str = "com.github.karinushka.paneru.launcher";

const IDENTITY_ENV: &str = "PANERU_CODESIGN_IDENTITY";
const AD_HOC: &str = "-";

/// The signing identity to use, taken from `PANERU_CODESIGN_IDENTITY`.
pub fn identity() -> String {
    resolve_identity(env::var(IDENTITY_ENV).ok())
}

fn resolve_identity(configured: Option<String>) -> String {
    configured
        .map(|identity| identity.trim().to_string())
        .filter(|identity| !identity.is_empty())
        .unwrap_or_else(|| AD_HOC.to_string())
}

/// Pins the signing identifier so the designated requirement does not depend on
/// the file name or the linker. `codesign` renames a new file over the target,
/// so signing a binary that is currently executing leaves the process alone.
pub fn sign(path: &Path, identifier: &str, identity: &str) -> Result<()> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--force", "--deep", "--sign", identity])
        .args(["--identifier", identifier])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(Error::other(format!(
            "codesign failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    if identity == AD_HOC {
        info!(
            "ad-hoc signed `{}`; the Accessibility grant will not survive a rebuild",
            path.display()
        );
    } else {
        info!("signed `{}` as `{identity}`", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_identity_falls_back_to_ad_hoc() {
        assert_eq!(resolve_identity(None), AD_HOC);
    }

    #[test]
    fn blank_identity_falls_back_to_ad_hoc() {
        assert_eq!(resolve_identity(Some("   ".to_string())), AD_HOC);
    }

    #[test]
    fn configured_identity_is_trimmed() {
        assert_eq!(
            resolve_identity(Some("  Paneru Self-Signed \n".to_string())),
            "Paneru Self-Signed"
        );
    }
}
