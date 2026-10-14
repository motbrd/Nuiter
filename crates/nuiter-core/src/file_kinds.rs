//! What a file is (by its name), and which files and folders are never read.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::uni::{ends_with_ignore_case, lower_str, starts_with_ignore_case};

/// Stored in the index as its number (the .NET enum's order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum FileCategory {
    #[default]
    None = 0,
    Document = 1,
    Pdf = 2,
    Image = 3,
    Audio = 4,
    Video = 5,
    Code = 6,
}

impl FileCategory {
    pub fn from_i32(v: i64) -> FileCategory {
        match v {
            1 => Self::Document,
            2 => Self::Pdf,
            3 => Self::Image,
            4 => Self::Audio,
            5 => Self::Video,
            6 => Self::Code,
            _ => Self::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Pdf => "PDF",
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Code => "Code",
            Self::None => "File",
        }
    }

    pub fn is_media(self) -> bool {
        matches!(self, Self::Image | Self::Audio | Self::Video)
    }
}

/// Path pieces the way .NET's `Path` names them: ".env" has the extension ".env" and an empty stem.
pub mod pathx {
    /// The last part of a path.
    pub fn file_name(path: &str) -> &str {
        let p = path.trim_end_matches(['\\', '/']);
        match p.rfind(['\\', '/']) {
            Some(i) => &p[i + 1..],
            None => p.strip_prefix(|c: char| c.is_ascii_alphabetic()).and_then(|r| r.strip_prefix(':')).unwrap_or(p),
        }
    }

    /// From the last dot of the file name (".jpg"), or "".
    pub fn extension(path: &str) -> &str {
        let name = file_name(path);
        match name.rfind('.') {
            Some(i) if i + 1 < name.len() => &name[i..],
            _ => "",
        }
    }

    /// The file name without its extension.
    pub fn stem(path: &str) -> &str {
        let name = file_name(path);
        match name.rfind('.') {
            Some(i) => &name[..i],
            None => name,
        }
    }

    /// The folder a path is in ("" at a root).
    pub fn parent(path: &str) -> &str {
        let p = path.trim_end_matches(['\\', '/']);
        match p.rfind(['\\', '/']) {
            // "C:\x" → "C:\" (a root keeps its separator, as .NET does).
            Some(i) if i == 2 && p.as_bytes().get(1) == Some(&b':') => &path[..3],
            Some(0) => &path[..1],
            Some(i) => &p[..i],
            None => "",
        }
    }

    pub fn join(dir: &str, name: &str) -> String {
        if dir.is_empty() {
            name.to_string()
        } else if dir.ends_with(['\\', '/']) {
            format!("{dir}{name}")
        } else {
            format!("{dir}\\{name}")
        }
    }

    /// A full, normalised path without a trailing separator (but "C:\" stays "C:\").
    pub fn normalize_dir(p: &str) -> String {
        let full = full_path(p);
        trim_end_sep(&full)
    }

    /// Without trailing separators, except for a drive's root ("C:\") or "\".
    pub fn trim_end_sep(p: &str) -> String {
        let t = p.trim_end_matches(['\\', '/']);
        if t.len() == 2 && t.as_bytes()[1] == b':' {
            format!("{t}\\")
        } else if t.is_empty() && !p.is_empty() {
            "\\".into()
        } else {
            t.to_string()
        }
    }

    /// The absolute form of a path (GetFullPathNameW: "..", "/" and relative parts resolved).
    pub fn full_path(p: &str) -> String {
        crate::win::full_path(p).unwrap_or_else(|| p.replace('/', "\\"))
    }

    /// Whether a path names a full location ("C:\x", "\\server\share"), not one relative to the current folder.
    pub fn is_fully_qualified(p: &str) -> bool {
        let b = p.as_bytes();
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
            || (b.len() >= 2 && (b[0] == b'\\' || b[0] == b'/') && (b[1] == b'\\' || b[1] == b'/'))
    }

    /// Whether `path` is `dir` itself or inside it (ignoring case).
    pub fn is_under(path: &str, dir: &str) -> bool {
        let d = trim_end_sep(dir);
        let Some(rest) = crate::uni::strip_prefix_ignore_case(path, &d) else { return false };
        rest.is_empty() || d.ends_with('\\') || rest.starts_with('\\')
    }

    /// Whether `path` is strictly inside `dir`.
    pub fn is_strictly_under(path: &str, dir: &str) -> bool {
        let d = trim_end_sep(dir);
        crate::uni::strip_prefix_ignore_case(path, &d)
            .is_some_and(|rest| !rest.is_empty() && (d.ends_with('\\') || rest.starts_with('\\')))
    }
}

pub const DOCUMENT_EXTS: &[&str] = &[
    ".txt", ".md", ".markdown", ".rst", ".org", ".log", ".csv", ".tsv", ".rtf", ".doc", ".docx", ".odt", ".pptx", ".odp",
    ".xlsx", ".ods", ".html", ".htm", ".xml", ".epub", ".eml", ".msg", ".tex", ".srt", ".vtt", ".json", ".yaml", ".yml",
];
pub const IMAGE_EXTS: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp", ".heic", ".heif", ".tif", ".tiff", ".jxr", ".ico", ".avif", ".dng",
    ".cr2", ".cr3", ".nef", ".arw", ".orf", ".rw2", ".raf",
];
pub const AUDIO_EXTS: &[&str] = &[".mp3", ".wav", ".m4a", ".aac", ".flac", ".wma", ".ogg", ".oga", ".opus", ".aiff", ".aif"];
pub const VIDEO_EXTS: &[&str] = &[".mp4", ".m4v", ".mov", ".avi", ".wmv", ".mkv", ".webm", ".3gp", ".mts", ".m2ts"];
pub const CODE_EXTS: &[&str] = &[
    ".cs", ".fs", ".vb", ".py", ".ipynb", ".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx", ".java", ".kt", ".kts", ".scala",
    ".go", ".rs", ".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hh", ".m", ".mm", ".swift", ".rb", ".php", ".lua", ".dart",
    ".r", ".jl", ".ex", ".exs", ".erl", ".hs", ".clj", ".sql", ".sh", ".bash", ".zsh", ".ps1", ".psm1", ".bat", ".cmd",
    ".vue", ".svelte", ".astro", ".css", ".scss", ".less", ".zig", ".nim", ".gd", ".proto", ".graphql", ".tf", ".cmake",
    ".gradle", ".xaml", ".razor", ".cshtml", ".md", ".rst", ".toml",
];

/// Folders never walked into (dot folders aside): package caches, environments and system folders. Names people also
/// give their own folders ("build", "out", "dist") are skipped only by code search.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules", "bower_components", "jspm_packages", "__pycache__", "site-packages", "venv", "$RECYCLE.BIN",
    "System Volume Information", "Config.Msi", "$WinREAgent", "Windows.old",
];

/// What code search also leaves out: build output, restored packages, test runs and code kept from elsewhere.
const CODE_SKIPPED_DIRS: &[&str] = &[
    "bin", "obj", "out", "build", "dist", "target", "artifacts", "packages", "TestResults", "coverage", "publish",
    "vendor", "third_party", "thirdparty", "external", "deps", "generated", "Generated Files",
];

/// Key and certificate stores that hold private keys.
const KEY_STORE_EXTS: &[&str] = &[
    ".pfx", ".p12", ".pem", ".key", ".ppk", ".pvk", ".snk", ".jks", ".keystore", ".kdbx", ".kdb", ".gpg", ".pgp", ".ovpn",
];

/// Files known by their whole name: SSH private keys, git and package-manager credentials, .NET user secrets.
const SECRET_NAMES: &[&str] = &[
    "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa", ".git-credentials", ".netrc", "_netrc", ".npmrc", ".pypirc", ".pgpass",
    "secrets.json", "credentials", "known_hosts",
];

const GENERATED_SUFFIXES: &[&str] =
    &[".min.js", ".min.css", ".g.cs", ".designer.cs", ".generated.cs", ".pb.go", "_pb2.py", ".lock", "-lock.json", ".map"];

/// Files that Windows runs when they're opened, and source code whose default action is to run it.
const RUN_EXTS: &[&str] = &[
    ".exe", ".com", ".scr", ".pif", ".cpl", ".msi", ".msp", ".msc", ".bat", ".cmd", ".ps1", ".psm1", ".psd1", ".vbs",
    ".vbe", ".vb", ".js", ".jse", ".mjs", ".cjs", ".wsf", ".wsh", ".ws", ".hta", ".reg", ".inf", ".lnk", ".url", ".scf",
    ".py", ".pyw", ".pyz", ".rb", ".pl", ".sh", ".jar", ".application", ".appref-ms", ".gadget", ".xbap",
    ".settingcontent-ms", ".library-ms", ".search-ms", ".appx", ".msix", ".appinstaller",
];

/// A folder holding any of these is the root of a code project: outside code search only its screenshots are indexed.
const PROJECT_EXTS: &[&str] =
    &[".sln", ".slnx", ".slnf", ".csproj", ".fsproj", ".vbproj", ".vcxproj", ".sqlproj", ".esproj", ".pyproj", ".xcodeproj"];
const PROJECT_FILES: &[&str] = &[
    ".git", "global.json", "Directory.Build.props", "vcpkg.json", "CMakeLists.txt", "meson.build", "package.json",
    "deno.json", "pyproject.toml", "setup.py", "requirements.txt", "Pipfile", "environment.yml", "Cargo.toml", "go.mod",
    "pom.xml", "build.gradle", "build.gradle.kts", "settings.gradle", "composer.json", "Gemfile", "mix.exs",
    "pubspec.yaml", "Package.swift", "project.godot",
];

const SCREENSHOT_NAMES: &[&str] = &["Screenshot", "Screen Shot", "Снимок экрана", "Скриншот", "CleanShot", "SCR-"];

fn lower_set(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| lower_str(s)).collect()
}

static CATEGORY_MAP: LazyLock<HashMap<String, FileCategory>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for e in DOCUMENT_EXTS {
        m.insert(e.to_string(), FileCategory::Document);
    }
    for e in IMAGE_EXTS {
        m.insert(e.to_string(), FileCategory::Image);
    }
    for e in AUDIO_EXTS {
        m.insert(e.to_string(), FileCategory::Audio);
    }
    for e in VIDEO_EXTS {
        m.insert(e.to_string(), FileCategory::Video);
    }
    m.insert(".pdf".into(), FileCategory::Pdf);
    m
});
static SKIPPED: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(SKIPPED_DIRS));
static CODE_SKIPPED: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(CODE_SKIPPED_DIRS));
static KEY_STORES: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(KEY_STORE_EXTS));
static SECRETS: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(SECRET_NAMES));
static RUNS: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(RUN_EXTS));
static CODE: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(CODE_EXTS));
static PROJECT_E: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(PROJECT_EXTS));
static PROJECT_F: LazyLock<HashSet<String>> = LazyLock::new(|| lower_set(PROJECT_FILES));

static SECRET_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[^a-z])(secrets?|passwords?|passwd|credentials?|private[-_ ]?key)([^a-z]|$)").unwrap()
});
static CAMERA_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(img|dsc|dscn|dscf|sam|pxl|mvimg|vid|video|mov|gopr|gp|dji|photo|wp|wa|whatsapp|screenshot|снимок|scan|signal)[-_ ]?\d").unwrap()
});
static DATE_TIME_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(19|20)\d{6}[-_ ]?\d{4,}").unwrap());

fn ext_lower(path: &str) -> String {
    lower_str(pathx::extension(path))
}

/// Category for a file found in a regular (non-code) folder.
pub fn classify(path: &str) -> FileCategory {
    CATEGORY_MAP.get(&ext_lower(path)).copied().unwrap_or(FileCategory::None)
}

pub fn is_code(path: &str) -> bool {
    let name = pathx::file_name(path);
    if is_secret_like(name) {
        return false;
    }
    if GENERATED_SUFFIXES.iter().any(|s| ends_with_ignore_case(name, s)) {
        return false;
    }
    CODE.contains(&ext_lower(path))
        || name.eq_ignore_ascii_case("Dockerfile")
        || name.eq_ignore_ascii_case("Makefile")
        || starts_with_ignore_case(name, "README")
}

/// Files that Windows runs when they're opened (programs, scripts, shortcuts, installers, registry files), and source
/// code whose default action is to run it. Search results of these kinds open as text, never with the shell's "open".
pub fn runs_when_opened(path: &str) -> bool {
    RUNS.contains(&ext_lower(path))
}

/// Never read, wherever it is: a store of private keys (by extension), a credential file known by its name, an
/// environment file (.env, .env.local), or a name that says it holds secrets or passwords. Public keys and
/// certificates (.pub, .crt, .cer) are not secret.
pub fn is_secret_like(file_name: &str) -> bool {
    KEY_STORES.contains(&ext_lower(file_name))
        || SECRETS.contains(&lower_str(file_name))
        || file_name.eq_ignore_ascii_case(".env")
        || starts_with_ignore_case(file_name, ".env.")
        || SECRET_WORD.is_match(pathx::stem(file_name))
}

/// Whether a folder holding these entries is the root of a code project.
pub fn is_code_project<'a>(entry_names: impl IntoIterator<Item = &'a str>) -> bool {
    entry_names.into_iter().any(|n| PROJECT_F.contains(&lower_str(n)) || PROJECT_E.contains(&ext_lower(n)))
}

/// Screenshots by their name (Snipping Tool, Win+PrtScn, CleanShot…).
pub fn is_screenshot_name(file_name: &str) -> bool {
    SCREENSHOT_NAMES.iter().any(|p| starts_with_ignore_case(file_name, p))
}

/// A file name's shape: lowercase, each run of digits as '#', each long hex run (a hash or a GUID's part) as 'h'.
/// "take_000123" and "take_004561" share "take_#"; "beach with anna" is its own.
pub fn name_shape(stem: &str) -> String {
    let lower: Vec<char> = lower_str(stem).chars().collect();
    // Hex runs: a whole token of [0-9a-z] that is all hex, 8+ long, with a letter and a digit.
    let mut replaced = String::with_capacity(lower.len());
    let mut i = 0;
    while i < lower.len() {
        let c = lower[i];
        if c.is_ascii_digit() || c.is_ascii_lowercase() {
            let start = i;
            while i < lower.len() && (lower[i].is_ascii_digit() || lower[i].is_ascii_lowercase()) {
                i += 1;
            }
            let token = &lower[start..i];
            let hex = token.iter().all(|c| c.is_ascii_hexdigit());
            if hex && token.len() >= 8 && token.iter().any(|c| c.is_ascii_digit()) && token.iter().any(|c| c.is_ascii_alphabetic())
            {
                replaced.push('h');
            } else {
                replaced.extend(token);
            }
        } else {
            replaced.push(c);
            i += 1;
        }
    }
    let mut out = String::with_capacity(replaced.len());
    for c in replaced.chars() {
        if !c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('#') {
            out.push('#');
        }
    }
    out
}

/// Names that cameras, phones, messengers and screenshot tools give: their numbering is someone's own photos, however
/// many there are. A leading word they use before a number, or a date and time ("20240501_183012").
pub fn is_camera_name(stem: &str) -> bool {
    CAMERA_NAME.is_match(stem) || DATE_TIME_NAME.is_match(stem)
}

/// Folders that are training data rather than someone's pictures, recordings or videos: at least `minimum` files of
/// one kind whose names nearly all (85%) share one numbered shape that isn't a camera's. They're left out and logged.
/// A folder chosen in Settings (`roots`, lowercase) is never one. Returns folder → files.
pub fn training_data_folders<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str, FileCategory)>,
    roots: &HashSet<String>,
    minimum: usize,
) -> Vec<(String, usize)> {
    // Groups in first-seen order, so ties break as .NET's MaxBy does.
    let mut order: Vec<(String, FileCategory)> = vec![];
    let mut groups: HashMap<(String, FileCategory), Vec<(&str, &str)>> = HashMap::new();
    for (folder, stem, cat) in files {
        if !cat.is_media() {
            continue;
        }
        let key = (lower_str(folder), cat);
        if roots.contains(&key.0) {
            continue;
        }
        let g = groups.entry(key.clone()).or_default();
        if g.is_empty() {
            order.push(key);
        }
        g.push((folder, stem));
    }
    let mut datasets: Vec<(String, usize)> = vec![];
    for key in order {
        let items = &groups[&key];
        if items.len() < minimum {
            continue;
        }
        let mut shapes: Vec<(String, Vec<&str>)> = vec![];
        let mut at: HashMap<String, usize> = HashMap::new();
        for (_, stem) in items {
            let s = name_shape(stem);
            match at.get(&s) {
                Some(&i) => shapes[i].1.push(stem),
                None => {
                    at.insert(s.clone(), shapes.len());
                    shapes.push((s, vec![stem]));
                }
            }
        }
        let mut top = &shapes[0];
        for s in &shapes {
            if s.1.len() > top.1.len() {
                top = s;
            }
        }
        let numbered = top.0.contains('#') || top.0.contains('h');
        let cameras = top.1.iter().filter(|s| is_camera_name(s)).count();
        if !numbered || top.1.len() * 100 < items.len() * 85 || cameras * 2 > top.1.len() {
            continue;
        }
        let folder = items[0].0.to_string();
        match datasets.iter_mut().find(|(f, _)| crate::uni::eq_ignore_case(f, &folder)) {
            Some(d) => d.1 += items.len(),
            None => datasets.push((folder, items.len())),
        }
    }
    datasets
}

pub fn should_skip_directory(name: &str, code: bool) -> bool {
    is_skipped_dir_name(name, code) || (name.starts_with('.') && name.len() > 1)
}

pub fn is_skipped_dir_name(name: &str, code: bool) -> bool {
    let l = lower_str(name);
    SKIPPED.contains(&l) || (code && CODE_SKIPPED.contains(&l))
}

/// Cloud placeholders (OneDrive "files on demand") and offline files are never read, so they are never downloaded.
pub fn is_cloud_placeholder(attributes: u32) -> bool {
    attributes & (0x0040_0000 /* RECALL_ON_DATA_ACCESS */ | 0x0004_0000 /* RECALL_ON_OPEN */ | 0x1000 /* OFFLINE */) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_have_shapes() {
        for (stem, shape) in [
            ("take_000123", "take_#"),
            ("shot-0042", "shot-#"),
            ("3f9a1c0e-77ab-4c2d", "h-#ab-#c#d"),
            ("a1b2c3d4e5f6", "h"),
            ("Beach with Anna", "beach with anna"),
        ] {
            assert_eq!(name_shape(stem), shape, "{stem}");
        }
    }

    #[test]
    fn camera_names_are_recognised() {
        for (stem, camera) in [
            ("IMG_4021", true),
            ("DSC01234", true),
            ("PXL_20240101_123456789", true),
            ("20240501_183012", true),
            ("WhatsApp Image 2024-05-01", false),
            ("take_000123", false),
            ("sample_0042", false),
        ] {
            assert_eq!(is_camera_name(stem), camera, "{stem}");
        }
    }

    #[test]
    fn dataset_folders_are_hundreds_of_media_files_named_one_numbered_way() {
        let mut files: Vec<(String, String, FileCategory)> = vec![];
        for i in 0..600 {
            files.push((r"C:\p\train".into(), format!("take_{i:06}"), FileCategory::Image));
            files.push((r"C:\p\phone".into(), format!("20240501_{}", 100000 + i), FileCategory::Image));
            files.push((r"C:\p\trip".into(), format!("IMG_{}", 4000 + i), FileCategory::Image));
            let mixed = if i % 3 == 0 { format!("frame_{i}") } else { format!("holiday {} by the sea", i % 7) };
            files.push((r"C:\p\mixed".into(), mixed, FileCategory::Image));
            files.push((r"C:\p\docs".into(), format!("{i:06}"), FileCategory::Document));
            files.push((r"C:\p".into(), format!("take_{i:06}"), FileCategory::Image));
        }
        for i in 0..50 {
            files.push((r"C:\p\few".into(), format!("take_{i:06}"), FileCategory::Image));
        }
        let roots: HashSet<String> = [r"c:\p".to_string()].into();
        let found = training_data_folders(files.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), *c)), &roots, 500);
        assert_eq!(found.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), vec![r"C:\p\train"]);
    }

    #[test]
    fn secret_files_are_never_read() {
        for (name, secret) in [
            ("deploy.ppk", true),
            ("signing.snk", true),
            ("secrets.yaml", true),
            ("db-password.txt", true),
            ("credentials.json", true),
            ("_netrc", true),
            ("id_ed25519", true),
            ("id_ed25519.pub", false),
            ("site.crt", false),
            ("passport scan.jpg", false),
            ("holiday.jpg", false),
            (".env", true),
            (".env.local", true),
            ("server.pem", true),
            ("id_rsa", true),
            ("environment.ts", false),
        ] {
            assert_eq!(is_secret_like(name), secret, "{name}");
        }
    }

    #[test]
    fn project_roots_are_found_by_their_markers() {
        assert!(is_code_project([".git", "README.md"]));
        assert!(is_code_project(["App.sln", "src"]));
        assert!(is_code_project(["package.json"]));
        assert!(!is_code_project(["Holiday", "notes.txt"]));
    }

    #[test]
    fn classifies_files() {
        assert_eq!(classify("photo.HEIC"), FileCategory::Image);
        assert_eq!(classify("report.pdf"), FileCategory::Pdf);
        assert_eq!(classify("song.flac"), FileCategory::Audio);
        assert_eq!(classify("clip.mkv"), FileCategory::Video);
        assert_eq!(classify("notes.docx"), FileCategory::Document);
        assert_eq!(classify("tool.exe"), FileCategory::None);
    }

    #[test]
    fn code_detection_skips_generated_files() {
        assert!(is_code(r"C:\r\src\main.rs"));
        assert!(!is_code(r"C:\r\dist\app.min.js"));
        assert!(!is_code(r"C:\r\Form1.Designer.cs"));
        assert!(is_code(r"C:\r\Dockerfile"));
    }

    #[test]
    fn files_that_would_run_are_recognised() {
        for (p, runs) in [
            (r"C:\repo\deploy.bat", true),
            (r"C:\repo\tool.py", true),
            (r"C:\repo\app.js", true),
            (r"C:\Downloads\setup.exe", true),
            (r"C:\Downloads\link.lnk", true),
            (r"C:\docs\report.pdf", false),
            (r"C:\docs\notes.md", false),
            (r"C:\photos\cat.jpg", false),
        ] {
            assert_eq!(runs_when_opened(p), runs, "{p}");
        }
    }

    #[test]
    fn path_pieces_follow_dotnet() {
        assert_eq!(pathx::extension(".env"), ".env");
        assert_eq!(pathx::stem(".env"), "");
        assert_eq!(pathx::extension(r"C:\a\b.tar.gz"), ".gz");
        assert_eq!(pathx::parent(r"C:\a\b.txt"), r"C:\a");
        assert_eq!(pathx::parent(r"C:\b.txt"), r"C:\");
        assert_eq!(pathx::file_name(r"C:\a\b\"), "b");
        assert!(pathx::is_strictly_under(r"C:\a\b\c.txt", r"C:\a\"));
        assert!(!pathx::is_strictly_under(r"C:\ab\c.txt", r"C:\a"));
        assert!(pathx::is_strictly_under(r"F:\x.txt", r"F:\"));
    }
}
