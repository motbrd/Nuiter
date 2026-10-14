//! The little HTTP/1.1 that talking to llama-server on 127.0.0.1 needs, over a plain socket: a GET for /health, and a
//! POST of /v1/embeddings whose body is written straight into the socket (pictures and sound as base64, never built as
//! one big string first).

use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::EmbedInput;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn connect(port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    let addr = ([127, 0, 0, 1], port).into();
    // No local port free for a moment (10048/10049): wait for one rather than fail the file.
    let mut tries = 0;
    let s = loop {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Err(e) if matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::AddrNotAvailable) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            r => break r?,
        }
    };
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.set_nodelay(true)?;
    Ok(s)
}

pub fn get(port: u16, path: &str, timeout: Duration) -> Result<Response> {
    let mut s = connect(port, timeout)?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")?;
    read_response(s)
}

/// POSTs a body of `length` bytes that `write_body` writes.
pub fn post(port: u16, path: &str, key: &str, length: u64, timeout: Duration, write_body: impl FnOnce(&mut dyn Write) -> std::io::Result<()>) -> Result<Response> {
    let s = connect(port, timeout)?;
    let mut w = BufWriter::with_capacity(64 << 10, s.try_clone()?);
    write!(
        w,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )?;
    write_body(&mut w)?;
    w.flush()?;
    drop(w);
    read_response(s)
}

fn read_response(s: TcpStream) -> Result<Response> {
    let mut r = BufReader::with_capacity(64 << 10, s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow!("bad HTTP response: {line:?}"))?;
    let mut length: Option<usize> = None;
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((name, value)) = l.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    let mut body = vec![];
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            let at = body.len();
            body.resize(at + size, 0);
            r.read_exact(&mut body[at..])?;
            line.clear();
            r.read_line(&mut line)?; // the chunk's CRLF
        }
    } else if let Some(n) = length {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
        return Ok(Response { status, body });
    }
    // Wait for llama-server to close first (it does at once after `Connection: close`): the side that closes first keeps
    // the connection in TIME_WAIT for minutes, and ours, holding a local port each, ran out of ports at ~30 files/s
    // (error 10048, the file marked unreadable).
    let _ = r.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
    let _ = r.read_to_end(&mut Vec::new());
    Ok(Response { status, body })
}

/// Counts what's written (the request's length, before it's sent).
struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The /v1/embeddings request: text as JSON strings, pictures as image_url data URLs, sound as input_audio WAV.
pub fn write_embeddings_request(w: &mut dyn Write, model: &str, inputs: &[EmbedInput]) -> std::io::Result<()> {
    write!(w, "{{\"model\":")?;
    serde_json::to_writer(&mut *w, model)?;
    write!(w, ",\"input\":[")?;
    for (i, input) in inputs.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        match input {
            EmbedInput::Text(t) => serde_json::to_writer(&mut *w, t)?,
            EmbedInput::Image(bytes) => {
                write!(w, "{{\"content\":[{{\"type\":\"image_url\",\"image_url\":{{\"url\":\"data:{};base64,", EmbedInput::image_mime(bytes))?;
                base64_to(w, bytes)?;
                write!(w, "\"}}}}]}}")?;
            }
            EmbedInput::Audio(wav) => {
                write!(w, "{{\"content\":[{{\"type\":\"input_audio\",\"input_audio\":{{\"data\":\"")?;
                base64_to(w, wav)?;
                write!(w, "\",\"format\":\"wav\"}}}}]}}")?;
            }
        }
    }
    write!(w, "]}}")
}

pub fn request_length(model: &str, inputs: &[EmbedInput]) -> u64 {
    let mut c = Counter(0);
    let _ = write_embeddings_request(&mut c, model, inputs);
    c.0
}

/// Base64 in pieces of 48 KB of input, so no buffer holds a whole picture's text.
fn base64_to(w: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    for piece in bytes.chunks(48 * 1024) {
        w.write_all(crate::vector_math::base64_encode(piece).as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_json_and_its_length_is_known() {
        let inputs = vec![EmbedInput::Text("a \"quoted\" ünïcode".into()), EmbedInput::Image(vec![0x89, 0x50, 1, 2, 3]), EmbedInput::Audio(vec![1; 100_000])];
        let mut body = vec![];
        write_embeddings_request(&mut body, "m", &inputs).unwrap();
        assert_eq!(body.len() as u64, request_length("m", &inputs));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["input"][0], "a \"quoted\" ünïcode");
        assert!(v["input"][1]["content"][0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,iVAB"));
        let audio = v["input"][2]["content"][0]["input_audio"]["data"].as_str().unwrap();
        assert_eq!(crate::vector_math::base64_decode(audio).unwrap(), vec![1; 100_000]);
    }
}
