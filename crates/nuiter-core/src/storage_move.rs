//! Moves the index or the model files to the folder chosen in Settings. The move runs at the start of the app, before
//! the index or a model server opens them; Settings asks for it with `move_index_to` / `move_models_to` and restarts.

use std::path::{Path, PathBuf};

use crate::file_kinds::pathx;
use crate::paths::{Log, Paths};
use crate::runtime::installer;
use crate::settings::Settings;

pub const INDEX_FILES: &[&str] = &["index.db", "index.db-wal", "index.db-shm", "index.db.reading"];
pub const MODEL_FILES: &[&str] = &[installer::MODEL_FILE, installer::MMPROJ_FILE];

/// Why `folder` can't take the index or the model files, or None when it can.
pub fn problem(folder: &str, current: &Path, files: &[&str]) -> Option<String> {
    if !pathx::is_fully_qualified(folder) {
        return Some(r"Choose a full path, like D:\Nuiter.".into());
    }
    if same(folder, &current.to_string_lossy()) {
        return Some("It's already there.".into());
    }
    if files.iter().any(|f| Path::new(folder).join(f).exists()) {
        return Some("That folder already has Nuiter's files in it.".into());
    }
    None
}

/// Why `folder` can't take the index, or None when it can: `problem`, and the index (SQLite with a write-ahead log)
/// can't live on a network share or a mapped network drive, where its locking doesn't hold.
pub fn index_problem(folder: &str, current: &Path) -> Option<String> {
    problem(folder, current, INDEX_FILES).or_else(|| {
        is_network(folder).then(|| "The index has to be on a drive of this PC, not on a network share.".to_string())
    })
}

fn is_network(folder: &str) -> bool {
    let full = pathx::full_path(folder);
    if full.starts_with(r"\\") {
        return true;
    }
    let Some(drive) = full.get(..3).filter(|d| d.as_bytes().get(1) == Some(&b':')) else { return false };
    // SAFETY: a null-terminated root path.
    unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(&windows::core::HSTRING::from(drive)) == 4 /* DRIVE_REMOTE */ }
}

/// Carries out the moves Settings asked for, saves the new folders, and returns what went wrong (the files stay where
/// they were then). `Paths::use_settings` must have the current folders; it gets the new ones afterwards.
pub fn apply_pending(s: &mut Settings) -> Vec<String> {
    let mut problems = vec![];
    if let Some(index) = s.move_index_to.take().filter(|t| !t.is_empty())
        && do_move(&Paths::index_dir(), &index, INDEX_FILES, "index", &mut problems) {
            s.index_directory =
                if same(&index, &Paths::home().to_string_lossy()) { String::new() } else { pathx::full_path(&index) };
        }
    if let Some(models) = s.move_models_to.take().filter(|t| !t.is_empty())
        && do_move(&Paths::models_dir(), &models, MODEL_FILES, "model files", &mut problems) {
            s.models_directory = if same(&models, &Paths::default_models_dir().to_string_lossy()) {
                String::new()
            } else {
                pathx::full_path(&models)
            };
        }
    if let Err(e) = s.save() {
        problems.push(format!("Settings couldn't be saved: {e}"));
    }
    Paths::use_settings(s);
    problems
}

fn do_move(from: &Path, to: &str, files: &[&str], what: &str, problems: &mut Vec<String>) -> bool {
    let problem = if files == INDEX_FILES { index_problem(to, from) } else { problem(to, from, files) };
    if let Some(why) = problem {
        problems.push(format!("The {what} wasn't moved to {to}: {why}"));
        return false;
    }
    let target = PathBuf::from(to);
    let mut moved: Vec<&str> = vec![];
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&target)?;
        for f in files {
            let source = from.join(f);
            if !source.exists() {
                continue;
            }
            move_file(&source, &target.join(f))?;
            moved.push(f);
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            Log::info(format!("Moved the {what} from {} to {to} ({})", from.display(), moved.join(", ")));
            true
        }
        Err(e) => {
            // Put back what already went, so the files are all in one place.
            for f in &moved {
                let _ = move_file(&target.join(f), &from.join(f));
            }
            Log::error(format!("Moving the {what} to {to} failed: {e}"));
            problems.push(format!("The {what} couldn't be moved to {to}: {e}"));
            false
        }
    }
}

/// A rename, or across drives a copy and a delete.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from).inspect_err(|_| {
        let _ = std::fs::remove_file(to);
    })
}

fn same(a: &str, b: &str) -> bool {
    crate::uni::eq_ignore_case(&pathx::trim_end_sep(&pathx::full_path(a)), &pathx::trim_end_sep(&pathx::full_path(b)))
}
