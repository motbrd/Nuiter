//! Downloads and verifies the pinned EmbeddingGemma 2 GGUF files (and ggml-org's llama.cpp build when llama-server isn't
//! bundled). Versions are pinned so that vectors stay comparable between releases; bump them deliberately (the index
//! stores the model id and re-embeds when it changes).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::cancel::Cancel;
use crate::paths::Paths;

pub const LLAMA_BUILD: &str = "b11577";
pub const MODEL_REPO: &str = "ggml-org/embeddinggemma-2-GGUF";
pub const MODEL_REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";
pub const MODEL_FILE: &str = "embeddinggemma-2-Q8_0.gguf";
pub const MMPROJ_FILE: &str = "mmproj-embeddinggemma-2-Q8_0.gguf";
/// Identifies the vector space. Changing it invalidates stored vectors.
pub const MODEL_ID: &str = "embeddinggemma-2-Q8_0";

#[derive(Debug, Clone)]
pub struct RuntimeFile {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: &'static str,
    /// "model" or "llama".
    pub kind: &'static str,
}

pub fn model_path() -> PathBuf {
    Paths::models_dir().join(MODEL_FILE)
}

pub fn mmproj_path() -> PathBuf {
    Paths::models_dir().join(MMPROJ_FILE)
}

/// llama-server built by scripts/build-llama.ps1 (the same tag plus a patch that stops it reserving a row of logits per
/// token, ~1 MiB each, which an embedding model never fills), shipped next to the app as llama\. Without it (ARM64, a
/// build without vendor\llama-bin) ggml-org's release is downloaded.
pub fn bundled_server_exe() -> PathBuf {
    Paths::exe_dir().join("llama").join("llama-server.exe")
}

pub fn is_bundled() -> bool {
    bundled_server_exe().is_file()
}

pub fn server_exe() -> PathBuf {
    if is_bundled() { bundled_server_exe() } else { Paths::llama_dir().join("llama-server.exe") }
}

pub fn flavor() -> String {
    if is_bundled() { format!("{LLAMA_BUILD}, patched") } else { LLAMA_BUILD.into() }
}

fn manifest_path() -> PathBuf {
    Paths::runtime().join("manifest.json")
}

pub fn files() -> Vec<RuntimeFile> {
    let hf = |f: &str| format!("https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_REVISION}/{f}");
    let mut list = vec![];
    if !is_bundled() {
        let arm = cfg!(target_arch = "aarch64");
        let (name, size, sha) = if arm {
            (format!("llama-{LLAMA_BUILD}-bin-win-vulkan-arm64.zip"), 24722170, "26390e83f2632b578850353df1f747291c4904708a6635c4d2c1d8f5a2d0c1a7")
        } else {
            (format!("llama-{LLAMA_BUILD}-bin-win-vulkan-x64.zip"), 32112073, "1d9a92d7bd92d41ace1ac21fb3ac2241a8175b932b9229b0d54e8bf625df8a81")
        };
        let url = format!("https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_BUILD}/{name}");
        list.push(RuntimeFile { name, url, size, sha256: sha, kind: "llama" });
    }
    list.push(RuntimeFile {
        name: MODEL_FILE.into(),
        url: hf(MODEL_FILE),
        size: 309855456,
        sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
        kind: "model",
    });
    list.push(RuntimeFile {
        name: MMPROJ_FILE.into(),
        url: hf(MMPROJ_FILE),
        size: 554821024,
        sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
        kind: "model",
    });
    list
}

pub fn total_download_bytes() -> u64 {
    files().iter().map(|f| f.size).sum()
}

/// The download's size as people read it: "865 MB".
pub fn download_size_text() -> String {
    format!("{} MB", (total_download_bytes() + 500_000) / 1_000_000)
}

fn read_manifest() -> BTreeMap<String, String> {
    std::fs::read(manifest_path())
        .ok()
        .and_then(|b| serde_json::from_slice(b.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&b)).ok())
        .unwrap_or_default()
}

fn save_manifest(m: &BTreeMap<String, String>) -> Result<()> {
    std::fs::write(manifest_path(), serde_json::to_string_pretty(m)?)?;
    Ok(())
}

static INSTALLED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Whether the model (and llama-server) are there and verified. Asked often: the answer is kept for two seconds.
pub fn is_installed() -> bool {
    let mut cache = INSTALLED.lock().unwrap();
    if let Some((at, v)) = *cache
        && at.elapsed() < Duration::from_secs(2)
    {
        return v;
    }
    let v = check_installed();
    *cache = Some((Instant::now(), v));
    v
}

fn forget_installed() {
    *INSTALLED.lock().unwrap() = None;
}

fn check_installed() -> bool {
    if !server_exe().is_file() || !model_path().is_file() || !mmproj_path().is_file() {
        return false;
    }
    let m = read_manifest();
    files().iter().all(|f| m.get(&f.name).is_some_and(|h| h == f.sha256))
}

/// Downloads whatever is missing. `progress` gets (message, 0..1).
pub fn install(progress: &dyn Fn(&str, f64), cancel: &Cancel) -> Result<()> {
    Paths::ensure();
    std::fs::create_dir_all(Paths::models_dir())?;
    let mut manifest = read_manifest();
    let files = files();
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut done: u64 = 0;
    for f in &files {
        let target = if f.kind == "model" { Paths::models_dir().join(&f.name) } else { Paths::runtime().join(&f.name) };
        let installed = if f.kind == "model" { target.is_file() } else { server_exe().is_file() };
        if installed && manifest.get(&f.name).is_some_and(|h| h == f.sha256) {
            done += f.size;
            continue;
        }
        // A model file may already be present (e.g. copied by hand): verified instead of downloaded again.
        if f.kind == "model" && std::fs::metadata(&target).is_ok_and(|m| m.len() == f.size) {
            progress(&format!("Verifying {}…", f.name), done as f64 / total as f64);
            if sha256_file(&target, cancel)? == f.sha256 {
                manifest.insert(f.name.clone(), f.sha256.into());
                save_manifest(&manifest)?;
                done += f.size;
                continue;
            }
        }

        let part = PathBuf::from(format!("{}.part", target.display()));
        let mut existing = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if existing > f.size {
            std::fs::remove_file(&part)?;
            existing = 0;
        }
        // A whole .part (the last run ended while verifying it, or the rename failed) isn't asked for again: a range
        // from its end is one the server can't satisfy (416), which failed every download after.
        if existing < f.size {
            download(f, &part, existing, &|got| {
                progress(&format!("Downloading {}: {} / {} MB", f.name, got >> 20, f.size >> 20), (done + got) as f64 / total as f64)
            }, cancel)?;
        }
        progress(&format!("Verifying {}…", f.name), (done + f.size) as f64 / total as f64);
        let hash = sha256_file(&part, cancel)?;
        if hash != f.sha256 {
            let _ = std::fs::remove_file(&part);
            bail!("Checksum mismatch for {} (expected {}, got {hash}).", f.name, f.sha256);
        }
        if f.kind == "llama" {
            let dir = Paths::llama_dir();
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            extract_zip(&part, &dir)?;
            std::fs::remove_file(&part)?;
        } else {
            if target.exists() {
                std::fs::remove_file(&target)?;
            }
            std::fs::rename(&part, &target)?;
        }
        manifest.insert(f.name.clone(), f.sha256.into());
        save_manifest(&manifest)?;
        done += f.size;
    }
    forget_installed();
    progress("Ready", 1.0);
    Ok(())
}

/// Fetches `f` into `part`, going on from the `existing` bytes already there when the server can (206); `progress`
/// gets the bytes so far.
fn download(f: &RuntimeFile, part: &std::path::Path, mut existing: u64, progress: &dyn Fn(u64), cancel: &Cancel) -> Result<()> {
    let headers = if existing > 0 { vec![format!("Range: bytes={existing}-")] } else { vec![] };
    let mut resp = crate::net::get(&f.url, &headers)?;
    if existing > 0 && resp.status == 416 {
        // The part doesn't fit what the server has: start over.
        drop(resp);
        std::fs::remove_file(part)?;
        existing = 0;
        resp = crate::net::get(&f.url, &[])?;
    }
    if !(200..300).contains(&resp.status) {
        bail!("{} answered {} for {}", host_of(&f.url), resp.status, f.name);
    }
    if existing > 0 && resp.status != 206 {
        existing = 0;
    }
    let mut out = std::fs::OpenOptions::new().create(true).write(true).append(existing > 0).truncate(existing == 0).open(part)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut got = existing;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        cancel.check()?;
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        got += n as u64;
        if last.elapsed() > Duration::from_millis(200) {
            last = Instant::now();
            progress(got);
        }
    }
    Ok(())
}

fn host_of(url: &str) -> &str {
    url.split('/').nth(2).unwrap_or(url)
}

pub fn sha256_file(path: &std::path::Path, cancel: &Cancel) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        cancel.check()?;
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(crate::win::hex(&h.finalize()))
}

/// Unpacks a zip into `dir`, refusing entries that would land outside it.
pub fn extract_zip(zip_path: &std::path::Path, dir: &std::path::Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(zip_path)?))?;
    std::fs::create_dir_all(dir)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().replace('\\', "/");
        let Some(rel) = entry.enclosed_name() else {
            return Err(anyhow!("the zip has an entry outside its folder: {name}"));
        };
        let rel: PathBuf = rel.components().collect();
        let out = dir.join(rel);
        if name.ends_with('/') {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut f = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}
