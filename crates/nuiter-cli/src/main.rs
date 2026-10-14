//! nuiter-cli: index, search and evaluate from a terminal. Uses the same index as the app unless NUITER_HOME is set (or
//! --home is given). `nuiter-cli extract-service` is a reader helper (the protocol runs over stdin/stdout).

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use nuiter_core::extract::host::ExtractionHost;
use nuiter_core::extract::protocol::CallBody;
use nuiter_core::file_kinds::{FileCategory, pathx};
use nuiter_core::index::forecast;
use nuiter_core::index::indexer::Indexer;
use nuiter_core::index::store::IndexStore;
use nuiter_core::index::vectors::{StoredVector, VectorIndex};
use nuiter_core::runtime::worker::{EmbeddingWorker, WorkerRole};
use nuiter_core::runtime::{EmbedInput, EmbedPriority, Embedder, installer, prompts, reference};
use nuiter_core::search::engine::{self, SearchEngine, SearchQuery};
use nuiter_core::{Cancel, Paths, Settings, vector_math, win};

/// Indexing stopped on a file that ran away (it is marked and left out): run index again to go on.
const STOPPED_EXIT_CODE: i32 = 3;

fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        return None;
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

fn full(p: &str) -> String {
    pathx::full_path(p)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // The app's reader helper: stdin/stdout carry its protocol, so nothing else may write to stdout.
    if args.len() == 1 && args[0] == "extract-service" {
        std::process::exit(nuiter_core::extract::service::run());
    }
    if let Some(home) = take_option(&mut args, "--home") {
        // SAFETY: set before any other thread exists.
        unsafe { std::env::set_var("NUITER_HOME", full(&home)) };
    }
    nuiter_core::cli_support::utf8_console();
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(mut args: Vec<String>) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!(
            "nuiter-cli: semantic file search with EmbeddingGemma 2

  nuiter-cli setup                       download EmbeddingGemma 2 (~900 MB)
  nuiter-cli index [folder...] [--code repo...]  index folders (default: folders from settings) and exit
  nuiter-cli search <query> [--type image|pdf|doc|audio|video|code] [-n 10] [--json]
  nuiter-cli status                      index statistics
  nuiter-cli estimate [folder...] [--code repo...]  files and first-index time (default: folders from settings)
  nuiter-cli eval <evals.json>           ranking quality (recall@k, MRR) for a list of queries
  nuiter-cli bench [--rounds 500] [--only search|pdf|video|load|read:<file>]  memory, leaks and latency
  nuiter-cli reference                   the CPU's reference vectors, and how the GPU compares
  --home <dir>                           use a separate data directory"
        );
        return Ok(0);
    }
    nuiter_core::win::init_com_mta();
    Paths::ensure();
    let mut settings = Settings::load();
    Paths::use_settings(&settings);
    let cancel = Cancel::new();

    match args[0].as_str() {
        "setup" => {
            setup(&cancel)?;
            Ok(0)
        }
        "index" => {
            let mut codes = vec![];
            while let Some(c) = take_option(&mut args, "--code") {
                codes.push(full(&c));
            }
            let folders: Vec<String> = args[1..].iter().map(|f| full(f)).collect();
            if !folders.is_empty() || !codes.is_empty() {
                settings.folders = folders;
                settings.code_folders = codes;
            }
            settings.pause_on_battery = false;
            settings.pause_on_energy_saver = false;
            settings.index_only_at_hours = false;
            if !installer::is_installed() {
                setup(&cancel)?;
            }
            let store = Arc::new(IndexStore::open_default()?);
            let vectors = Arc::new(VectorIndex::default());
            let s2 = settings.clone();
            let worker = EmbeddingWorker::new(move || s2.clone(), WorkerRole::Index);
            let reader = ExtractionHost::try_create();
            let indexer = Indexer::new(store, vectors, Arc::new(worker.clone()), settings, reader.clone());
            let last = std::sync::Mutex::new(String::new());
            indexer.on_status(move |s| {
                let current = s.current.as_deref().map(|c| format!("· {}", pathx::file_name(c))).unwrap_or_default();
                let line = format!("[{:?}] queued {} · files {} · vectors {} · errors {} {current}", s.phase, s.queued, s.files, s.chunks, s.errors);
                let mut l = last.lock().unwrap();
                if *l != line {
                    println!("{line}");
                    *l = line;
                }
            });
            // A file that made reading run away is marked unreadable; run index again to go on without it.
            indexer.on_stopped(|why| {
                eprintln!("Stopped: {why}. Run it again to go on.");
                std::process::exit(STOPPED_EXIT_CODE);
            });
            let sw = Instant::now();
            indexer.start();
            let _ = indexer.wait_for_idle(&cancel);
            let st = indexer.status();
            let secs = sw.elapsed().as_secs();
            println!("Done in {:02}:{:02}: {} files, {} vectors, {} errors", secs / 60, secs % 60, st.files, st.chunks, st.errors);
            indexer.cancel();
            if let Some(r) = reader {
                r.shutdown();
            }
            worker.dispose();
            indexer.stop();
            Ok(0)
        }
        "search" => {
            let kind = take_option(&mut args, "--type");
            let n: usize = take_option(&mut args, "-n").and_then(|v| v.parse().ok()).unwrap_or(10);
            let json = match args.iter().position(|a| a == "--json") {
                Some(i) => {
                    args.remove(i);
                    true
                }
                None => false,
            };
            let q = args[1..].join(" ");
            let (store, vectors) = open(true)?;
            let s2 = settings.clone();
            let worker = EmbeddingWorker::new(move || s2.clone(), WorkerRole::Query);
            let engine = SearchEngine::new(store, vectors, Arc::new(worker.clone()));
            let code = kind.as_deref() == Some("code");
            let mut query = SearchQuery::new(&if code { format!("code: {q}") } else { q });
            query.category = kind.and_then(|t| engine::parse(&format!("type:{t}"), None).category);
            query.limit = n;
            let results = engine.search(&query, &cancel)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for r in &results {
                    let loc = r.locator.as_deref().map(|l| format!("  [{l}]")).unwrap_or_default();
                    println!("{:6.2}  {:<8} {:<8} {}{loc}", r.score, format!("{:?}", r.match_kind), r.category.label(), r.path);
                    if let Some(s) = &r.snippet {
                        println!("        {}", s.chars().take(140).collect::<String>());
                    }
                }
            }
            worker.dispose();
            Ok(0)
        }
        "status" => {
            let store = IndexStore::open_default()?;
            let (files, chunks, errors, partial) = store.stats();
            println!("Data:     {}", Paths::home().display());
            println!("Index:    {}", Paths::database().display());
            println!("Models:   {}", Paths::models_dir().display());
            println!(
                "Runtime:  {}",
                if installer::is_installed() {
                    format!("installed (llama.cpp {}, {})", installer::flavor(), installer::MODEL_ID)
                } else {
                    "not installed".into()
                }
            );
            println!("Files:    {files} ({partial} PDFs partially indexed), {errors} errors");
            println!("Vectors:  {chunks}");
            println!("Folders:  {}", settings.folders.join("; "));
            Ok(0)
        }
        "estimate" => {
            let mut codes = vec![];
            while let Some(c) = take_option(&mut args, "--code") {
                codes.push(full(&c));
            }
            let mut folders: Vec<String> = args[1..].iter().map(|f| full(f)).collect();
            if folders.is_empty() && codes.is_empty() {
                folders = settings.folders.clone();
                codes = settings.code_folders.clone();
            }
            let (store, vectors) = open(false)?;
            let s2 = settings.clone();
            let worker = EmbeddingWorker::new(move || s2.clone(), WorkerRole::Index);
            let indexer = Indexer::new(store, vectors, Arc::new(worker), settings.clone(), ExtractionHost::try_create());
            for (folder, code) in folders.iter().map(|f| (f, false)).chain(codes.iter().map(|f| (f, true))) {
                let e = indexer.estimate_folder(folder, code, &cancel)?;
                let mut kinds: Vec<(FileCategory, usize)> = e.by_kind.iter().map(|(k, v)| (*k, *v)).collect();
                kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
                let kinds: Vec<String> = kinds.iter().map(|(k, v)| format!("{v} {}", k.label().to_lowercase())).collect();
                println!("{folder}{}", if code { " (code)" } else { "" });
                println!(
                    "  {} files ({}), {} already indexed, {} PDF pages, {:.1} h of audio/video",
                    e.files,
                    kinds.join(", "),
                    e.indexed,
                    e.pdf_pages,
                    e.media_seconds / 3600.0
                );
                let mut line = format!("  first pass ≈ {}", forecast::duration_text(e.seconds));
                if e.rest_seconds >= 1.0 {
                    line += &format!(", then ≈ {} for the rest of long PDFs", forecast::duration_text(e.rest_seconds));
                }
                if code {
                    line += &format!(", index ≈ {}", forecast::size_text(e.bytes));
                }
                if e.guessed_seconds >= 1.0 {
                    line += &format!(" ({} of it from default costs, not measured on this PC)", forecast::duration_text(e.guessed_seconds));
                }
                println!("{line}");
            }
            indexer.stop();
            Ok(0)
        }
        "reference" => {
            // The CPU's vectors for the reference text and picture (paste them into runtime/reference.rs), and how the
            // GPU compares.
            if !installer::is_installed() {
                setup(&cancel)?;
            }
            let probes = vec![EmbedInput::Text(prompts::document(None, reference::TEXT)), EmbedInput::Image(reference::image())];
            let mut cpu_settings = settings.clone();
            cpu_settings.backend = "cpu".into();
            let cpu = EmbeddingWorker::new(move || cpu_settings.clone(), WorkerRole::Index);
            let r = cpu.embed(&probes, EmbedPriority::Query, &cancel)?;
            cpu.dispose();
            println!("TEXT_VECTOR  = \"{}\"", vector_math::base64_encode(&vector_math::to_blob(&r[0])));
            println!("IMAGE_VECTOR = \"{}\"", vector_math::base64_encode(&vector_math::to_blob(&r[1])));
            let mut gpu_settings = settings.clone();
            gpu_settings.backend = "gpu".into();
            let gpu = EmbeddingWorker::new(move || gpu_settings.clone(), WorkerRole::Index);
            let g = gpu.embed(&probes, EmbedPriority::Query, &cancel)?;
            println!(
                "GPU ({}) vs CPU: text {:.5}, image {:.5}",
                gpu.device(),
                vector_math::dot(&g[0], &r[0]),
                vector_math::dot(&g[1], &r[1])
            );
            gpu.dispose();
            Ok(0)
        }
        "eval" => eval(args.get(1).map(String::as_str).unwrap_or("evals/queries.json"), &settings, &cancel),
        "bench" => {
            let rounds = take_option(&mut args, "--rounds").and_then(|v| v.parse().ok()).unwrap_or(500);
            let only = take_option(&mut args, "--only");
            bench(rounds, only.as_deref(), &settings, &cancel)
        }
        other => {
            eprintln!("Unknown command '{other}'. Try 'nuiter-cli help'.");
            Ok(2)
        }
    }
}

fn open(load_vectors: bool) -> Result<(Arc<IndexStore>, Arc<VectorIndex>)> {
    let store = Arc::new(IndexStore::open_default()?);
    let vectors = Arc::new(VectorIndex::default());
    if load_vectors {
        let mut all: Vec<StoredVector> = vec![];
        store.load_vectors(&mut |v| all.push(v));
        vectors.add(all);
    }
    Ok((store, vectors))
}

fn setup(cancel: &Cancel) -> Result<()> {
    println!("Downloading the model ({} MB) to {}", installer::total_download_bytes() >> 20, Paths::models_dir().display());
    let last = std::sync::Mutex::new(-1i64);
    installer::install(
        &|msg, fraction| {
            let pct = (fraction * 100.0) as i64;
            let mut l = last.lock().unwrap();
            if pct != *l {
                *l = pct;
                println!("{pct:3}%  {msg}");
            }
        },
        cancel,
    )?;
    println!("Runtime ready.");
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalCase {
    query: String,
    expect: Vec<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn eval(file: &str, settings: &Settings, cancel: &Cancel) -> Result<i32> {
    // [{ "query": "...", "expect": ["file-name-substring", ...], "type": "image" }]
    let cases: Vec<EvalCase> = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    let (store, vectors) = open(true)?;
    let s2 = settings.clone();
    let worker = EmbeddingWorker::new(move || s2.clone(), WorkerRole::Query);
    let engine = SearchEngine::new(store, vectors, Arc::new(worker.clone()));
    let (mut mrr, mut hit1, mut hit5, mut folded, mut shown) = (0.0, 0, 0, 0, 0usize);
    for c in &cases {
        let mut q = SearchQuery::new(&c.query);
        q.category = c.kind.as_ref().and_then(|t| engine::parse(&format!("type:{t}"), None).category);
        q.limit = 20;
        let results = engine.search(&q, cancel)?;
        let rank = results
            .iter()
            .position(|r| c.expect.iter().any(|e| nuiter_core::uni::contains_ignore_case(&r.path, e)))
            .map(|i| i + 1)
            .unwrap_or(0);
        if rank > 0 {
            mrr += 1.0 / rank as f64;
        }
        if rank == 1 {
            hit1 += 1;
        }
        if rank > 0 && rank <= 5 {
            hit5 += 1;
        }
        // What the search window shows before its fold ("14 more results"): the answer should be in it.
        let strong = engine::shown_count(&results);
        shown += strong;
        if rank > strong {
            folded += 1;
        }
        let mark = if rank == 1 { "✔" } else if rank > 0 { "~" } else { "✘" };
        let top = results.first().map(|r| pathx::file_name(&r.path).to_string()).unwrap_or_else(|| "(nothing)".into());
        println!(
            "{mark} #{:<3} {}   → {top}{}",
            if rank == 0 { "-".to_string() } else { rank.to_string() },
            c.query,
            if rank > strong { "   (folded)" } else { "" }
        );
    }
    let n = cases.len().max(1) as f64;
    println!(
        "\nqueries {}  recall@1 {:.0}%  recall@5 {:.0}%  MRR {:.3}  shown {:.1}  folded answers {folded}",
        cases.len(),
        hit1 as f64 / n * 100.0,
        hit5 as f64 / n * 100.0,
        mrr / n,
        shown as f64 / n
    );
    worker.dispose();
    Ok(0)
}

/// Searches again and again, with the previews the search window would ask for (PDF pages, video frames, through the
/// reader helper), printing memory and handles as it goes: what keeps growing is a leak. Then search latency.
fn bench(rounds: usize, only: Option<&str>, settings: &Settings, cancel: &Cancel) -> Result<i32> {
    let queries = [
        "кошка", "собака на пляже", "снег в горах", "договор", "чек из магазина", "закат на море", "birthday cake", "скриншот с ошибкой",
        "видео с ребенком", "invoice", "буфер прыжка", "паспорт", "INV-2026-0042", "code: retry",
    ];
    let (store, vectors) = open(true)?;
    let reader = ExtractionHost::try_create();
    let s2 = settings.clone();
    let worker = EmbeddingWorker::new(move || s2.clone(), WorkerRole::Query);
    let engine = SearchEngine::new(store.clone(), vectors.clone(), Arc::new(worker.clone()));
    let stats = |label: &str| {
        nuiter_core::memory::relieve();
        let helpers = reader.as_ref().map(|r| r.helpers()).unwrap_or((0, 0, 0));
        println!(
            "{label:<12} private {:>4} MB · working set {:>4} MB · helpers {} ({} MB, {} handles)",
            win::private_bytes() >> 20,
            win::working_set() >> 20,
            helpers.0,
            helpers.1 >> 20,
            helpers.2
        );
    };
    println!("{} vectors, reader {}", vectors.count(), if reader.is_some() { "helpers" } else { "in-process" });
    stats("start");
    if only == Some("load") {
        for _ in 0..3 {
            let sw = Instant::now();
            let mut list = vec![];
            store.load_vectors(&mut |v| list.push(v));
            let read = sw.elapsed().as_millis();
            let n = list.len();
            let idx = VectorIndex::default();
            idx.add(list);
            println!("load {n}: read {read} ms, add {} ms", sw.elapsed().as_millis() - read);
        }
        let q = worker.embed(&[EmbedInput::Text(prompts::query("кошка"))], EmbedPriority::Query, cancel)?.remove(0);
        for n in [100_000usize, 500_000, 1_000_000] {
            let big = VectorIndex::with_capacity(768, n);
            let mut seed = 1u64;
            let mut batch = Vec::with_capacity(10_000);
            for j in 0..n {
                let mut v = vec![0f32; 768];
                for x in v.iter_mut() {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    *x = ((seed >> 33) as f64 / (1u64 << 31) as f64 - 0.5) as f32;
                }
                vector_math::normalize(&mut v);
                batch.push(StoredVector::new(j as i64, (j / 20) as i64, "text", false, &v));
                if batch.len() == 10_000 {
                    big.add(std::mem::take(&mut batch));
                }
            }
            big.search(&q, false);
            let sw = Instant::now();
            for _ in 0..5 {
                big.search(&q, false);
            }
            println!("search over {n} vectors: {} ms ({} MB)", sw.elapsed().as_millis() / 5, big.bytes() >> 20);
        }
        stats("end");
        worker.dispose();
        return Ok(0);
    }
    if let Some(file) = only.and_then(|o| o.strip_prefix("read:"))
        && let Some(r) = &reader
    {
        for i in 1..=rounds {
            let ctx = nuiter_core::extract::ExtractContext::new(file, settings.clone());
            let mut n = 0;
            r.extract(&ctx, nuiter_core::file_kinds::classify(file), cancel, &mut |_| {
                n += 1;
                Ok(())
            })?;
            if i == 1 || i % 5 == 0 {
                stats(&format!("read {i} ({n})"));
            }
        }
        return Ok(0);
    }
    let mut latencies = vec![];
    for i in 1..=rounds {
        let q = queries[i % queries.len()];
        let sw = Instant::now();
        let results = engine.search(&SearchQuery::new(q), cancel)?;
        latencies.push(sw.elapsed().as_secs_f64() * 1000.0);
        // What the preview pane asks for when a result is selected.
        if let Some(r) = &reader
            && only != Some("search")
        {
            for res in results.iter().take(3) {
                if res.category == FileCategory::Pdf && matches!(only, None | Some("pdf")) {
                    let mut call = CallBody::new("pdf-page", &res.path);
                    call.page = res.position.unwrap_or(1.0) as usize;
                    call.query = Some(q.into());
                    call.pixels = 1100;
                    r.call(&call, std::time::Duration::from_secs(15));
                } else if res.category == FileCategory::Video && matches!(only, None | Some("video")) {
                    let mut call = CallBody::new("video-frame", &res.path);
                    call.seconds = res.position.unwrap_or(0.0);
                    call.pixels = 640;
                    r.call(&call, std::time::Duration::from_secs(15));
                }
            }
        }
        if i == 10 || i % 100 == 0 {
            stats(&format!("round {i}"));
        }
    }
    latencies.drain(..latencies.len().min(5)); // the model starting
    latencies.sort_by(f64::total_cmp);
    if !latencies.is_empty() {
        let p = |q: f64| latencies[((q * latencies.len() as f64) as usize).min(latencies.len() - 1)];
        println!("search latency: p50 {:.0} ms · p95 {:.0} ms · max {:.0} ms", p(0.5), p(0.95), latencies[latencies.len() - 1]);
    }
    worker.dispose();
    Ok(0)
}
