//! The reader helper's side: reads requests from stdin until it closes, one file at a time, and answers on stdout.

use std::io::{BufReader, BufWriter, Write};

use super::protocol::{self, CallBody, EndBody, FailedBody, RequestBody, ResultBody};
use super::{ExtractContext, preview};
use crate::cancel::Cancel;
use crate::paths::Log;

pub fn run() -> i32 {
    crate::win::init_com_mta();
    Log::never_rotate();
    // A panic in a decoder is that file's failure; the log says what it was.
    std::panic::set_hook(Box::new(|info| Log::error(format!("Reader helper {}: {info}", std::process::id()))));
    Log::info(format!("Reader helper {} started", std::process::id()));
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = BufWriter::with_capacity(256 << 10, std::io::stdout().lock());
    while let Ok(Some((kind, payload))) = protocol::read_frame(&mut input) {
        let ok = match kind {
            protocol::CALL => handle_call(&payload, &mut output),
            protocol::REQUEST => handle_request(&payload, &mut output),
            _ => Ok(()),
        };
        if ok.is_err() {
            break; // the app is gone
        }
        // Whatever the decoders freed goes back to Windows before the next file.
        crate::memory::relieve();
    }
    Log::info(format!("Reader helper {} done", std::process::id()));
    0
}

fn failed(output: &mut impl Write, kind: &str, message: String) -> std::io::Result<()> {
    protocol::write_json(output, protocol::FAILED, &FailedBody { kind: kind.into(), message })
}

fn handle_call(payload: &[u8], output: &mut impl Write) -> std::io::Result<()> {
    let call: CallBody = match serde_json::from_slice(payload) {
        Ok(c) => c,
        Err(e) => return failed(output, "BadCall", e.to_string()),
    };
    fault(&call.path);
    match std::panic::catch_unwind(|| run_call(&call)) {
        Ok(Ok(r)) => protocol::write_frame(output, protocol::RESULT, &protocol::encode_result(&r)),
        Ok(Err(e)) => failed(output, "Error", e.to_string()),
        Err(_) => failed(output, "Panic", "the reader failed".into()),
    }
}

fn run_call(call: &CallBody) -> anyhow::Result<ResultBody> {
    Ok(match call.op.as_str() {
        "pdf-pages" => ResultBody { pages: super::pdf::page_count(&call.path), ..Default::default() },
        "pdf-landmark" => ResultBody {
            word: crate::search::pdf_jump::find(&call.path, call.page, call.query.as_deref().unwrap_or("")),
            ..Default::default()
        },
        "pdf-page" => match preview::pdf_page(&call.path, call.page, call.query.as_deref().unwrap_or(""), call.pixels) {
            Some((image, marks)) => ResultBody { marks: Some(marks), image: Some(image), ..Default::default() },
            None => ResultBody::default(),
        },
        "video-frame" => ResultBody { image: preview::video_frame(&call.path, call.seconds, call.pixels), ..Default::default() },
        other => anyhow::bail!("unknown call {other}"),
    })
}

fn handle_request(payload: &[u8], output: &mut impl Write) -> std::io::Result<()> {
    let request: RequestBody = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return failed(output, "BadRequest", e.to_string()),
    };
    let ctx = ExtractContext {
        path: request.path.clone(),
        settings: request.settings,
        start_unit: request.start_unit,
        unit_budget: request.unit_budget,
    };
    fault(&ctx.path);
    let mut write_error: Option<std::io::Error> = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::extract(&ctx, request.category, &Cancel::never(), &mut |item| {
            if let Err(e) = protocol::write_frame(output, protocol::ITEM, &protocol::encode_item(&item)) {
                write_error = Some(std::io::Error::new(e.kind(), e.to_string()));
                anyhow::bail!("the app stopped listening");
            }
            Ok(())
        })
    }));
    if let Some(e) = write_error {
        return Err(e);
    }
    match result {
        Ok(Ok(end)) => protocol::write_json(output, protocol::END, &EndBody { next_unit: end.next_unit, title: end.title }),
        Ok(Err(e)) => failed(output, "Error", format!("{e:#}")),
        Err(_) => failed(output, "Panic", "the reader failed".into()),
    }
}

/// For tests: NUITER_READER_FAULT="crash:<part of a path>" or "hang:<part of a path>" makes the helper crash or hang
/// on that file, as a broken decoder would.
fn fault(path: &str) {
    let Ok(fault) = std::env::var("NUITER_READER_FAULT") else { return };
    let (what, part) = fault.split_once(':').unwrap_or((&fault, ""));
    if part.is_empty() || !crate::uni::contains_ignore_case(path, part) {
        return;
    }
    if what == "crash" {
        std::process::abort();
    }
    if what == "hang" {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}
