//! The pinned Chrome for Testing: where it lives, and `shakedown browser install`.
//!
//! One exact build, downloaded from Google's Chrome for Testing bucket, checked against a SHA-256 kept in this
//! tool, unpacked with macOS `ditto` (an `.app` bundle holds symlinks and code signatures that `ditto` keeps) into
//! `~/Library/Caches/shakedown/chrome/<version>/`, shared by every project. Bumping the version is a tool release
//! plus a reviewed `update --screens all` in each project.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::error::Error;

/// The Chrome for Testing build every reference is made with.
pub const CHROME_VERSION: &str = "149.0.7827.55";
/// SHA-256 of `chrome-mac-arm64.zip` for [`CHROME_VERSION`].
const CHROME_SHA256: &str = "311211b54c429245e2cec0314ee1e314085e9c00350215b95e1a879350786630";

fn url() -> String {
    format!(
        "https://storage.googleapis.com/chrome-for-testing-public/{CHROME_VERSION}/mac-arm64/chrome-mac-arm64.zip"
    )
}

/// `$SHAKEDOWN_CACHE`, else `~/Library/Caches/shakedown`.
pub fn cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SHAKEDOWN_CACHE") {
        return PathBuf::from(p);
    }
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join("Library/Caches/shakedown")
}

/// The directory the pinned build unpacks into.
pub fn chrome_dir() -> PathBuf {
    cache_dir().join("chrome").join(CHROME_VERSION)
}

/// The Chrome executable.
pub fn binary() -> PathBuf {
    chrome_dir().join(
        "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    )
}

/// The pinned build is installed.
pub fn installed() -> bool {
    binary().is_file()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// SHA-256 of a file, streamed.
///
/// # Errors
///
/// The file cannot be read.
pub fn sha256_file(path: &std::path::Path) -> Result<String, Error> {
    let io = |e: std::io::Error| Error::Environment(format!("{}: {e}", path.display()));
    let mut f = std::fs::File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Installs the pinned Chrome if it is missing; returns the executable. Safe to run again and from several
/// sessions at once (the unpacked tree is renamed into place, never built in place).
///
/// # Errors
///
/// Not an Apple Silicon Mac, the download fails or does not match the pinned checksum, or unpacking fails.
pub fn install() -> Result<PathBuf, Error> {
    if installed() {
        return Ok(binary());
    }
    if !(cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")) {
        return Err(Error::Environment(
            "the pinned Chrome for Testing is mac-arm64 only (Todd's Mac)".into(),
        ));
    }
    let env = |what: &str, e: &dyn std::fmt::Display| Error::Environment(format!("{what}: {e}"));
    let root = cache_dir().join("chrome");
    std::fs::create_dir_all(&root).map_err(|e| env("cannot create the cache", &e))?;
    let work = root.join(format!(".install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| env("cannot create the cache", &e))?;
    let outcome = (|| {
        eprintln!("installing Chrome for Testing {CHROME_VERSION} (once, about 180 MB)...");
        let zip = work.join("chrome.zip");
        let st = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&zip)
            .arg(url())
            .status()
            .map_err(|e| env("curl", &e))?;
        if !st.success() {
            return Err(Error::Environment(format!("download failed: {}", url())));
        }
        let got = sha256_file(&zip)?;
        if got != CHROME_SHA256 {
            return Err(Error::Environment(format!(
                "the download does not match the pinned checksum (got {got}); not installing"
            )));
        }
        let unpacked = work.join("unpacked");
        let st = Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&zip)
            .arg(&unpacked)
            .status()
            .map_err(|e| env("ditto", &e))?;
        if !st.success() {
            return Err(Error::Environment("ditto could not unpack Chrome".into()));
        }
        if let Err(e) = std::fs::rename(&unpacked, chrome_dir()) {
            // Another session finished first: fine, as long as it is there.
            if !installed() {
                return Err(env("cannot move Chrome into place", &e));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    outcome?;
    if installed() {
        Ok(binary())
    } else {
        Err(Error::Environment(
            "Chrome unpacked but its executable is missing".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lower_case_and_padded() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn the_pin_is_well_formed() {
        assert_eq!(CHROME_SHA256.len(), 64);
        assert!(url().contains(CHROME_VERSION));
        assert!(binary().starts_with(chrome_dir()));
    }

    #[test]
    fn sha256_of_a_known_file() {
        let p = std::env::temp_dir().join(format!("shakedown-sha-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_file(&p).unwrap();
    }
}
