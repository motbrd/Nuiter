//! Opens results at the matching page, timestamp or line when the target app supports it. Opening never runs
//! anything: scripts, programs and shortcuts open as text.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use nuiter_core::extract::protocol::CallBody;
use nuiter_core::file_kinds::{self, FileCategory, pathx};
use nuiter_core::search::SearchResult;
use nuiter_core::{Log, Settings};
use windows::Win32::UI::Shell::{ASSOCF_NOTRUNCATE, ASSOCSTR_EXECUTABLE, AssocQueryStringW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PWSTR, w};

const NO_WINDOW: u32 = 0x0800_0000;

/// `query`: what was searched; a PDF opens with one of its words searched for.
pub fn open(r: &SearchResult, s: &Settings, query: &str) {
    let done = (|| -> anyhow::Result<bool> {
        if r.category == FileCategory::Code && try_editor(&s.code_editor, &r.path, r.position.unwrap_or(1.0) as usize) {
            return Ok(true);
        }
        // Code and scripts are shown, never run: a .py, .bat or .js found in someone's repo must not execute.
        if r.category == FileCategory::Code || file_kinds::runs_when_opened(&r.path) {
            open_as_text(&r.path, s);
            return Ok(true);
        }
        if r.category == FileCategory::Pdf
            && let Some(page) = r.position
            && try_pdf_at_page(&r.path, page as usize, query)
        {
            return Ok(true);
        }
        if matches!(r.category, FileCategory::Audio | FileCategory::Video)
            && let Some(sec) = r.position
            && try_media_at(&r.path, sec)
        {
            return Ok(true);
        }
        Ok(false)
    })();
    if !matches!(done, Ok(true)) {
        if let Err(e) = done {
            Log::warn(format!("Open failed for {}: {e}", r.path));
        }
        shell_open(&r.path, s);
    }
}

/// Opens a file in its app, as a double-click in Explorer does, except for files that would run (scripts, programs,
/// shortcuts): those open as text.
pub fn shell_open(path: &str, s: &Settings) {
    if file_kinds::runs_when_opened(path) {
        open_as_text(path, s);
        return;
    }
    // SAFETY: the shell's "open" on a file path.
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path), None, None, SW_SHOWNORMAL);
    }
}

/// A file shown as text: the code editor from Settings when it's there, else Notepad.
fn open_as_text(path: &str, s: &Settings) {
    if try_editor(&s.code_editor, path, 1) {
        return;
    }
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let _ = Command::new(format!(r"{windir}\System32\notepad.exe")).arg(path).spawn();
}

pub fn reveal(path: &str) {
    let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

pub fn open_folder(path: &str) {
    // SAFETY: the shell's "open" on a folder.
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path), None, None, SW_SHOWNORMAL);
    }
}

/// An editor in a folder of PATH. Only full paths count: a relative entry ("." or "bin") would name a folder relative to
/// wherever Nuiter was started (as `git` is looked for by the indexer).
fn find_on_path(name: &str) -> Option<String> {
    std::env::var("PATH")
        .ok()?
        .split(';')
        .map(str::trim)
        .filter(|d| pathx::is_fully_qualified(d))
        .map(|d| pathx::join(d, name))
        .find(|p| std::path::Path::new(p).is_file())
}

fn try_editor(editor: &str, path: &str, line: usize) -> bool {
    let e = editor.trim().to_lowercase();
    if e.is_empty() {
        return false;
    }
    let exe = match e.as_str() {
        "code" | "vscode" => find_on_path("code.cmd").or_else(|| find_on_path("code")),
        "cursor" => find_on_path("cursor.cmd").or_else(|| find_on_path("cursor")),
        "windsurf" => find_on_path("windsurf.cmd"),
        "zed" => find_on_path("zed.exe").or_else(|| find_on_path("zed")),
        "rider" => find_on_path("rider64.exe").or_else(|| find_on_path("rider.cmd")),
        "sublime" | "subl" => find_on_path("subl.exe"),
        "notepad++" => Some(r"C:\Program Files\Notepad++\notepad++.exe".into()),
        _ => find_on_path(editor),
    };
    let Some(exe) = exe.filter(|x| std::path::Path::new(x).is_file()) else { return false };
    let mut cmd = Command::new(&exe);
    match e.as_str() {
        "rider" => {
            cmd.arg("--line").arg(line.to_string()).arg(path);
        }
        "notepad++" => {
            cmd.arg(format!("-n{line}")).arg(path);
        }
        // VS Code, Cursor, Windsurf, Zed, Sublime.
        _ => {
            cmd.arg("-g").arg(format!("{path}:{line}"));
        }
    }
    cmd.creation_flags(NO_WINDOW).spawn().is_ok()
}

/// How long opening a PDF may wait for its search word (reading earlier pages of a long PDF).
const LANDMARK_TIMEOUT: Duration = Duration::from_millis(1500);

fn try_pdf_at_page(path: &str, page: usize, query: &str) -> bool {
    let Some(exe) = assoc_exe(".pdf") else { return false };
    let name = pathx::file_name(&exe).to_lowercase();
    let chromium = matches!(name.as_str(), "msedge.exe" | "chrome.exe" | "brave.exe" | "vivaldi.exe" | "opera.exe");
    let known = chromium
        || matches!(
            name.as_str(),
            "firefox.exe" | "acrobat.exe" | "acrord32.exe" | "sumatrapdf.exe" | "foxitpdfreader.exe" | "foxitphantompdf.exe" | "foxitpdfeditor.exe"
        );
    if !known {
        return false;
    }
    // A word from the query that's on the page (and preferably on no earlier one): the viewer searches for it,
    // highlights it, and its first match is on the page that matched.
    let mut word: Option<String> = None;
    if !query.trim().is_empty() {
        // In the reader helper when there is one: reading earlier pages of a broken PDF can't hang the app.
        word = match crate::app::core().reader.clone() {
            Some(reader) => {
                let mut call = CallBody::new("pdf-landmark", path);
                call.page = page;
                call.query = Some(query.into());
                reader.call(&call, LANDMARK_TIMEOUT).and_then(|r| r.word)
            }
            None => nuiter_core::search::pdf_jump::find(path, page, query),
        };
    }
    // The word comes from the PDF and goes into a command line as it is (Acrobat, Sumatra): only a plain word may.
    let word = word.filter(|w| w.chars().all(|c| nuiter_core::uni::is_letter_or_digit(c) || nuiter_core::uni::is_mark(c) || c == '_'));
    let url = file_url(path);
    let mut cmd = Command::new(&exe);
    let args = if chromium {
        match &word {
            Some(w) => format!("\"{url}#page={page}:~:text={}\"", percent(w)),
            None => format!("\"{url}#page={page}\""),
        }
    } else if name == "firefox.exe" {
        match &word {
            Some(w) => format!("\"{url}#page={page}&search={}\"", percent(w)),
            None => format!("\"{url}#page={page}\""),
        }
    } else if name == "acrobat.exe" || name == "acrord32.exe" {
        match &word {
            Some(w) => format!("/A \"page={page}&search={w}\" \"{path}\""),
            None => format!("/A \"page={page}\" \"{path}\""),
        }
    } else if name == "sumatrapdf.exe" {
        match &word {
            Some(w) => format!("-page {page} -search \"{w}\" \"{path}\""),
            None => format!("-page {page} \"{path}\""),
        }
    } else {
        format!("\"{path}\" /A page={page}") // Foxit
    };
    Log::info(format!(
        "Open {} at page {page}{} in {name}",
        pathx::file_name(path),
        word.as_deref().map(|w| format!(", searching “{w}”")).unwrap_or_default()
    ));
    cmd.raw_arg(args).spawn().is_ok()
}

/// "file:///C:/My%20Docs/a.pdf": every byte outside the unreserved set and '/' ':' escaped, as a URI's path.
fn file_url(path: &str) -> String {
    let p = path.replace('\\', "/");
    let mut out = String::from("file:///");
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether the app that opens `path` can be told where in a recording to start.
pub fn opens_at_moment(path: &str) -> bool {
    assoc_exe(pathx::extension(path)).is_some_and(|exe| moment_args(&exe, path, 0.0).is_some())
}

fn try_media_at(path: &str, seconds: f64) -> bool {
    let Some(exe) = assoc_exe(pathx::extension(path)) else { return false };
    let Some(args) = moment_args(&exe, path, seconds) else { return false };
    Command::new(&exe).raw_arg(args).spawn().is_ok()
}

fn moment_args(exe: &str, path: &str, seconds: f64) -> Option<String> {
    let s = seconds as i64;
    Some(match pathx::file_name(exe).to_lowercase().as_str() {
        "vlc.exe" => format!("--start-time={s} \"{path}\""),
        "mpv.exe" => format!("--start={s} \"{path}\""),
        "mpc-hc64.exe" | "mpc-hc.exe" | "mpc-be64.exe" => format!("\"{path}\" /start {}", s * 1000),
        "potplayermini64.exe" | "potplayermini.exe" => format!("\"{path}\" /seek={s}"),
        _ => return None,
    })
}

fn assoc_exe(ext: &str) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer and its length are passed together.
    let r = unsafe { AssocQueryStringW(ASSOCF_NOTRUNCATE, ASSOCSTR_EXECUTABLE, &HSTRING::from(ext), w!("open"), Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if r.is_err() {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..n]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_urls_escape_what_a_uri_must() {
        assert_eq!(super::file_url(r"C:\My Docs\счёт #1.pdf"), "file:///C:/My%20Docs/%D1%81%D1%87%D1%91%D1%82%20%231.pdf");
    }
}
