//! New versions from GitHub Releases. A release's zip is the one scripts/publish.ps1 makes,
//! `Nuiter-{version}-win-{arch}.zip`. Nothing installs by itself: the user clicks, the zip must match the SHA-256 GitHub
//! lists for the asset (a release without one isn't installed), and it is unpacked into a new folder beside the running
//! one, so the old version stays until the new one runs.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::cancel::Cancel;
use crate::paths::{Log, Paths};

/// A newer release: its version, page, and the zip for this machine with the SHA-256 GitHub gives for it.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateInfo {
    pub version: Version,
    pub page_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub sha256: Option<String>,
    pub size: u64,
}

/// major.minor.patch (a fourth part is read and ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let parts: Vec<&str> = text.trim().trim_start_matches(['v', 'V']).split('.').collect();
        if parts.len() < 2 || parts.len() > 4 {
            return None;
        }
        let n: Vec<u32> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
        Some(Version(n[0], n[1], *n.get(2).unwrap_or(&0)))
    }

    pub fn current() -> Version {
        Version::parse(crate::VERSION).unwrap_or(Version(0, 0, 0))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" }
}

pub static REPOSITORY_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$").unwrap());

/// The latest release of `repository` ("owner/name") if it's newer than this one and has a zip for this machine; None
/// otherwise. Fails when GitHub can't say (no public release, offline).
pub fn check(repository: &str) -> Result<Option<UpdateInfo>> {
    if !REPOSITORY_PATTERN.is_match(repository) {
        bail!("\"{repository}\" isn't owner/name.");
    }
    let (status, body) = crate::net::get_text(
        &format!("https://api.github.com/repos/{repository}/releases/latest"),
        &["Accept: application/vnd.github+json".into()],
    )?;
    // 404: no such public repository, or no release in it yet (a private one needs a sign-in Nuiter doesn't have).
    if status == 404 {
        bail!("GitHub has no public release of {repository}");
    }
    if !(200..300).contains(&status) {
        bail!("GitHub answered {status}");
    }
    parse(&body, repository, Version::current(), arch())
}

/// The update in a GitHub "latest release" response, if it's newer than `current`.
pub fn parse(json: &str, repository: &str, current: Version, arch: &str) -> Result<Option<UpdateInfo>> {
    let root: serde_json::Value = serde_json::from_str(json)?;
    if root["draft"].as_bool() == Some(true) || root["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let tag = root["tag_name"].as_str().unwrap_or("");
    let Some(version) = Version::parse(tag) else { return Ok(None) };
    if version <= current {
        return Ok(None);
    }
    let name = format!("Nuiter-{version}-win-{arch}.zip");
    // Only a file of this release, from GitHub itself.
    let prefix = format!("https://github.com/{repository}/releases/download/");
    for asset in root["assets"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if !asset["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let url = asset["browser_download_url"].as_str().unwrap_or("");
        if !crate::uni::starts_with_ignore_case(url, &prefix) {
            return Ok(None);
        }
        let sha = asset["digest"]
            .as_str()
            .and_then(|d| crate::uni::strip_prefix_ignore_case(d, "sha256:"))
            .map(str::to_lowercase);
        return Ok(Some(UpdateInfo {
            version,
            page_url: root["html_url"].as_str().unwrap_or("").into(),
            asset_name: name,
            asset_url: url.into(),
            sha256: sha,
            size: asset["size"].as_u64().unwrap_or(0),
        }));
    }
    Ok(None)
}

/// Downloads the zip, checks its SHA-256, and unpacks it into a folder next to `app_dir` (`Nuiter-{version}-win-{arch}`).
/// Returns the new Nuiter.exe.
pub fn install(update: &UpdateInfo, app_dir: &Path, progress: &dyn Fn(f64), cancel: &Cancel) -> Result<PathBuf> {
    let Some(expected) = &update.sha256 else {
        bail!("The release lists no SHA-256 for its zip, so it can't be checked.");
    };
    std::fs::create_dir_all(Paths::temp())?;
    let zip = Paths::temp().join(&update.asset_name);
    let result = (|| -> Result<PathBuf> {
        let mut resp = crate::net::get(&update.asset_url, &[])?;
        if !(200..300).contains(&resp.status) {
            bail!("GitHub answered {}", resp.status);
        }
        let total = resp.length.unwrap_or(update.size);
        let mut out = std::fs::File::create(&zip)?;
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            cancel.check()?;
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            done += n as u64;
            if total > 0 {
                progress(done as f64 / total as f64);
            }
        }
        drop(out);
        let actual = crate::win::hex(&hash.finalize());
        if actual != *expected {
            bail!("The download doesn't match the release's SHA-256 ({actual}).");
        }
        let parent = app_dir.parent().ok_or_else(|| anyhow!("no folder above {}", app_dir.display()))?;
        let target = parent.join(update.asset_name.trim_end_matches(".zip"));
        let partial = PathBuf::from(format!("{}.partial", target.display()));
        if partial.exists() {
            std::fs::remove_dir_all(&partial)?;
        }
        super::installer::extract_zip(&zip, &partial)?;
        // Releases hold one folder (Nuiter\Nuiter.exe, Nuiter\llama\…); older ones had the files at the top.
        let root = if partial.join("Nuiter.exe").is_file() {
            partial.clone()
        } else {
            let dirs: Vec<PathBuf> = std::fs::read_dir(&partial)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            match dirs.as_slice() {
                [only] if only.join("Nuiter.exe").is_file() => only.clone(),
                _ => bail!("The zip has no Nuiter.exe."),
            }
        };
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::rename(&root, &target)?;
        if root != partial {
            let _ = std::fs::remove_dir_all(&partial);
        }
        Log::info(format!("Update {} unpacked to {}", update.version, target.display()));
        Ok(target.join("Nuiter.exe"))
    })();
    let _ = std::fs::remove_file(&zip);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, asset_url: &str, digest: Option<&str>, prerelease: bool) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/me/Nuiter/releases/tag/{tag}","draft":false,"prerelease":{prerelease},
             "assets":[{{"name":"Nuiter-0.8.0-win-x64.zip","browser_download_url":"{asset_url}","size":123{}}},
                       {{"name":"Nuiter-0.8.0-win-arm64.zip","browser_download_url":"https://github.com/me/Nuiter/releases/download/v0.8.0/arm.zip","size":1}}]}}"#,
            digest.map(|d| format!(",\"digest\":\"{d}\"")).unwrap_or_default()
        )
    }

    const GOOD: &str = "https://github.com/me/Nuiter/releases/download/v0.8.0/Nuiter-0.8.0-win-x64.zip";

    #[test]
    fn a_newer_release_gives_this_machines_zip_and_its_checksum() {
        let u = parse(&release("v0.8.0", GOOD, Some("sha256:ABC123"), false), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().unwrap();
        assert_eq!(u.version, Version(0, 8, 0));
        assert_eq!(u.asset_name, "Nuiter-0.8.0-win-x64.zip");
        assert_eq!(u.asset_url, GOOD);
        assert_eq!(u.sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn old_versions_prereleases_and_files_from_elsewhere_are_not_offered() {
        let d = Some("sha256:ABC123");
        assert!(parse(&release("v0.7.0", GOOD, d, false), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().is_none());
        assert!(parse(&release("v0.6.9", GOOD, d, false), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().is_none());
        assert!(parse(&release("v0.8.0", GOOD, d, true), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().is_none());
        assert!(parse(&release("v0.8.0", "https://evil.example/Nuiter-0.8.0-win-x64.zip", d, false), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().is_none());
        assert!(parse(&release("v0.8.0", GOOD, d, false), "someone/else", Version(0, 7, 0), "x64").unwrap().is_none());
        assert!(parse(&release("v0.8.0", GOOD, None, false), "me/Nuiter", Version(0, 7, 0), "x64").unwrap().unwrap().sha256.is_none());
        assert!(!REPOSITORY_PATTERN.is_match("me/Nuiter/../x"));
        assert!(REPOSITORY_PATTERN.is_match("motionbeard/Nuiter"));
    }
}
