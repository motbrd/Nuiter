include!("../../build-support/copy_llama.rs");

fn main() {
    copy_llama();
    // The icon Explorer shows, the manifest (per-monitor DPI, long paths, UTF-8, the segment heap, themed controls) and
    // the version.
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let parts: Vec<&str> = v.split('.').collect();
    let commas = format!("{},{},{},0", parts[0], parts.get(1).unwrap_or(&"0"), parts.get(2).unwrap_or(&"0"));
    // Plain quotes: Command escapes them for rc.exe's command line itself (an escaped \" reached rc as a backslash).
    let text = format!("\"{v}\"");
    println!("cargo:rerun-if-changed=assets/nuiter.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/nuiter.ico");
    // Required: without these resources the exe has no icon and runs without its manifest.
    embed_resource::compile("assets/nuiter.rc", [format!("NUITER_VERSION_COMMAS={commas}"), format!("NUITER_VERSION_TEXT={text}")])
        .manifest_required()
        .unwrap();
}
