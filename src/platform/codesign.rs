//! Signing for the binaries Paneru installs.
//!
//! An ad-hoc signature makes a binary's designated requirement its own cdhash,
//! which is what TCC stores when Accessibility is granted. Every rebuild changes
//! the cdhash and silently voids the grant. A stable certificate moves the
//! requirement onto the certificate instead.

use std::{
    env, fs,
    io::{Error, Result},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{self, Command},
};

use tracing::{info, warn};

pub const SERVICE_IDENTIFIER: &str = "com.github.karinushka.paneru";
pub const LAUNCHER_IDENTIFIER: &str = "com.github.karinushka.paneru.launcher";

const IDENTITY_ENV: &str = "PANERU_CODESIGN_IDENTITY";
const AD_HOC: &str = "-";
const IDENTITY_NAME: &str = "Paneru Self-Signed";
/// Twenty years. Rotating it invalidates every grant issued against the old one.
const IDENTITY_DAYS: &str = "7300";

/// The signing identity to use. `PANERU_CODESIGN_IDENTITY` wins when set, and
/// `-` selects ad-hoc. Otherwise Paneru reuses its own certificate, issuing it
/// on first use and falling back to ad-hoc if that fails, so an uncooperative
/// keychain degrades rather than failing the install.
pub fn identity() -> String {
    if let Some(configured) = configured_identity(env::var(IDENTITY_ENV).ok()) {
        return configured;
    }
    match ensure_identity() {
        Ok(()) => IDENTITY_NAME.to_string(),
        Err(err) => {
            warn!("could not prepare a signing certificate: {err}");
            AD_HOC.to_string()
        }
    }
}

fn configured_identity(configured: Option<String>) -> Option<String> {
    configured
        .map(|identity| identity.trim().to_string())
        .filter(|identity| !identity.is_empty())
}

fn ensure_identity() -> Result<()> {
    if identity_exists()? {
        return Ok(());
    }
    create_identity()
}

fn identity_exists() -> Result<bool> {
    let output = Command::new("/usr/bin/security")
        .args(["find-identity", "-p", "codesigning"])
        .output()?;
    Ok(lists_identity(
        &String::from_utf8_lossy(&output.stdout),
        IDENTITY_NAME,
    ))
}

/// A self-signed root is reported untrusted and counted as zero valid
/// identities, yet is still listed and still signs, so match on the name.
fn lists_identity(output: &str, name: &str) -> bool {
    output
        .lines()
        .any(|line| line.contains(&format!("\"{name}\"")))
}

fn create_identity() -> Result<()> {
    let workspace = scratch_dir()?;
    let key = workspace.join("signing.key");
    let certificate = workspace.join("signing.crt");

    let result = issue_certificate(&key, &certificate).and_then(|()| {
        import(&key)?;
        import(&certificate)
    });
    fs::remove_dir_all(&workspace)?;
    result?;

    info!("issued signing certificate `{IDENTITY_NAME}` into the login keychain");
    Ok(())
}

fn scratch_dir() -> Result<PathBuf> {
    let workspace = env::temp_dir().join(format!("paneru-signing-{}", process::id()));
    if workspace.exists() {
        fs::remove_dir_all(&workspace)?;
    }
    fs::create_dir_all(&workspace)?;
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700))?;
    Ok(workspace)
}

fn issue_certificate(key: &Path, certificate: &Path) -> Result<()> {
    run(
        "/usr/bin/openssl",
        &[
            "req".as_ref(),
            "-x509".as_ref(),
            "-newkey".as_ref(),
            "rsa:2048".as_ref(),
            "-nodes".as_ref(),
            "-days".as_ref(),
            IDENTITY_DAYS.as_ref(),
            "-subj".as_ref(),
            format!("/CN={IDENTITY_NAME}").as_ref(),
            "-addext".as_ref(),
            "extendedKeyUsage=critical,codeSigning".as_ref(),
            // Omit this and codesign reports "no identity found".
            "-addext".as_ref(),
            "keyUsage=critical,digitalSignature".as_ref(),
            "-keyout".as_ref(),
            key.as_os_str(),
            "-out".as_ref(),
            certificate.as_os_str(),
        ],
    )
}

/// Importing the key and certificate as separate PEMs lets the keychain pair
/// them, avoiding a PKCS#12 file and its transport password.
fn import(path: &Path) -> Result<()> {
    run(
        "/usr/bin/security",
        &[
            "import".as_ref(),
            path.as_os_str(),
            "-k".as_ref(),
            login_keychain()?.as_os_str(),
            "-T".as_ref(),
            "/usr/bin/codesign".as_ref(),
        ],
    )
}

fn login_keychain() -> Result<PathBuf> {
    let home = env::home_dir().ok_or(Error::other("cannot find home directory"))?;
    Ok(home.join("Library/Keychains/login.keychain-db"))
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = Command::new(program).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::other(format!(
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
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
    fn unset_identity_defers_to_the_issued_certificate() {
        assert_eq!(configured_identity(None), None);
        assert_eq!(configured_identity(Some("   ".to_string())), None);
    }

    #[test]
    fn configured_identity_is_trimmed() {
        assert_eq!(
            configured_identity(Some("  Custom Identity \n".to_string())),
            Some("Custom Identity".to_string())
        );
    }

    #[test]
    fn ad_hoc_stays_selectable() {
        assert_eq!(
            configured_identity(Some(AD_HOC.to_string())),
            Some(AD_HOC.to_string())
        );
    }

    #[test]
    fn untrusted_self_signed_roots_still_count_as_listed() {
        let output = concat!(
            "  1) AAAA \"Some Other Identity\"\n",
            "  2) BBBB \"Paneru Self-Signed\" (CSSMERR_TP_NOT_TRUSTED)\n",
            "     2 identities found\n",
            "     0 valid identities found\n",
        );
        assert!(lists_identity(output, IDENTITY_NAME));
        assert!(!lists_identity(output, "Paneru Self"));
        assert!(!lists_identity("     0 identities found", IDENTITY_NAME));
    }
}
