// Shared by the executables' build scripts: the patched llama-server (scripts/build-llama.ps1, in vendor/llama-bin)
// goes next to the executable as llama\, so a dev build runs the same server a release ships. x64 only; without it,
// the app downloads ggml-org's build instead.

fn copy_llama() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("..").join("..").join("vendor").join("llama-bin");
    println!("cargo:rerun-if-changed={}", source.join("BUILD").display());
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") || !source.join("llama-server.exe").is_file() {
        return;
    }
    // OUT_DIR is target/<profile>/build/<crate>-<hash>/out: the executable is three folders up.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(bin) = out.ancestors().nth(3) else { return };
    let target = bin.join("llama");
    let stamp = std::fs::read_to_string(source.join("BUILD")).unwrap_or_default();
    if std::fs::read_to_string(target.join("BUILD")).unwrap_or_default() == stamp && target.join("llama-server.exe").is_file() {
        return;
    }
    let _ = std::fs::create_dir_all(&target);
    if let Ok(entries) = std::fs::read_dir(&source) {
        for e in entries.flatten() {
            if e.path().is_file() {
                let _ = std::fs::copy(e.path(), target.join(e.file_name()));
            }
        }
    }
}
