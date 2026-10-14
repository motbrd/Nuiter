//! E-mails saved as .eml (RFC 5322 with MIME): the subject, who wrote to whom and when, and the text of the message
//! (its plain-text part, else its HTML without markup), decoded from base64 or quoted-printable and from its
//! charset. Attachments are named, not read; a forwarded message (message/rfc822) is read like the outer one.

use super::document_text::{decode, strip_html};
use crate::uni::{eq_ignore_case, lower_str, starts_with_ignore_case};

/// A part's headers, unfolded, names as written.
type Headers = Vec<(String, String)>;

/// Parts nested deeper than this are left out (a crafted file could nest without end).
const MAX_DEPTH: usize = 8;

/// The text of an e-mail.
pub fn eml(raw: &[u8]) -> String {
    let mut out = String::new();
    let mut attachments: Vec<String> = vec![];
    message(raw, 0, &mut out, &mut attachments);
    if !attachments.is_empty() {
        out.push_str("\nAttachments: ");
        out.push_str(&attachments.join(", "));
        out.push('\n');
    }
    out
}

/// A whole message: the headers people read, then its body.
fn message(raw: &[u8], depth: usize, out: &mut String, attachments: &mut Vec<String>) {
    let (headers, body) = split_headers(raw);
    for name in ["Subject", "From", "To", "Cc", "Date"] {
        if let Some(v) = header(&headers, name) {
            out.push_str(&format!("{name}: {}\n", decode_words(&v)));
        }
    }
    out.push('\n');
    part(&headers, body, depth, out, attachments);
}

/// One MIME part, from its headers and body.
fn part(headers: &[(String, String)], body: &[u8], depth: usize, out: &mut String, attachments: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let content_type = header(headers, "Content-Type").unwrap_or_else(|| "text/plain".into());
    let mime = lower_str(content_type.split(';').next().unwrap_or("").trim());
    let disposition = header(headers, "Content-Disposition").unwrap_or_default();
    let name = parameter(&disposition, "filename").or_else(|| parameter(&content_type, "name")).map(|n| decode_words(&n));
    if starts_with_ignore_case(disposition.trim(), "attachment") || (name.is_some() && !mime.starts_with("multipart/")) {
        if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
            attachments.push(n);
        }
        return;
    }
    if let Some(sub) = mime.strip_prefix("multipart/") {
        let Some(boundary) = parameter(&content_type, "boundary") else { return };
        let parts: Vec<(Headers, &[u8])> = split_multipart(body, &boundary).into_iter().map(split_headers).collect();
        if sub == "alternative" {
            // The same message more than once: the plain text when there is one, else the richest that's last.
            let is = |h: &[(String, String)], t: &str| {
                header(h, "Content-Type").is_some_and(|c| starts_with_ignore_case(c.trim(), t))
            };
            let pick = parts.iter().find(|(h, _)| is(h, "text/plain")).or_else(|| parts.last());
            if let Some((h, b)) = pick {
                part(h, b, depth + 1, out, attachments);
            }
            return;
        }
        for (h, b) in &parts {
            part(h, b, depth + 1, out, attachments);
        }
        return;
    }
    if mime == "message/rfc822" {
        message(&transfer_decode(headers, body), depth + 1, out, attachments);
        return;
    }
    if mime == "text/plain" || mime == "text/html" || mime.is_empty() {
        let text = charset_decode(&transfer_decode(headers, body), parameter(&content_type, "charset").as_deref());
        if mime == "text/html" {
            out.push_str(&strip_html(&text));
        } else {
            out.push_str(&text);
        }
        out.push('\n');
    }
}

/// Headers (unfolded, names as written) and the body after the blank line.
fn split_headers(raw: &[u8]) -> (Headers, &[u8]) {
    // A part with no headers starts with its blank line.
    if let Some(body) = raw.strip_prefix(b"\r\n").or_else(|| raw.strip_prefix(b"\n")) {
        return (vec![], body);
    }
    let mut end = raw.len();
    let mut body_at = raw.len();
    for i in 0..raw.len() {
        if raw[i] == b'\n' {
            let rest = &raw[i + 1..];
            if rest.starts_with(b"\r\n") {
                (end, body_at) = (i, i + 3);
                break;
            }
            if rest.starts_with(b"\n") {
                (end, body_at) = (i, i + 2);
                break;
            }
        }
    }
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut headers: Vec<(String, String)> = vec![];
    for line in head.split('\n').map(|l| l.trim_end_matches('\r')) {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    (headers, &raw[body_at.min(raw.len())..])
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers.iter().find(|h| eq_ignore_case(&h.0, name)).map(|h| h.1.clone())
}

/// A parameter of a header value: `boundary="abc"`, `charset=utf-8` (RFC 2231's `name*=` is read as plain `name=`).
fn parameter(value: &str, name: &str) -> Option<String> {
    for p in value.split(';').skip(1) {
        let Some((k, v)) = p.split_once('=') else { continue };
        let k = k.trim().trim_end_matches('*');
        if !eq_ignore_case(k, name) {
            continue;
        }
        let v = v.trim();
        // name*=utf-8''r%C3%A9sum%C3%A9.pdf
        if let Some((_, encoded)) = v.split_once("''") {
            return Some(String::from_utf8_lossy(&percent_decode(encoded)).into_owned());
        }
        return Some(v.trim_matches('"').to_string());
    }
    None
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// The parts between `--boundary` lines, up to `--boundary--`.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}");
    let d = delimiter.as_bytes();
    let mut parts = vec![];
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < body.len() {
        let line_end = body[i..].iter().position(|&c| c == b'\n').map_or(body.len(), |p| i + p);
        let line = trim_cr(&body[i..line_end]);
        if line.starts_with(d) {
            if let Some(s) = start {
                // The part ends before the line break that precedes the delimiter.
                parts.push(trim_cr_lf_end(&body[s..i]));
            }
            if line[d.len()..].starts_with(b"--") {
                return parts;
            }
            start = Some((line_end + 1).min(body.len()));
        }
        i = line_end + 1;
    }
    if let Some(s) = start {
        parts.push(&body[s.min(body.len())..]);
    }
    parts
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_cr_lf_end(b: &[u8]) -> &[u8] {
    let b = b.strip_suffix(b"\n").unwrap_or(b);
    b.strip_suffix(b"\r").unwrap_or(b)
}

fn transfer_decode(headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let encoding = lower_str(header(headers, "Content-Transfer-Encoding").unwrap_or_default().trim());
    match encoding.as_str() {
        "base64" => base64_lenient(body),
        "quoted-printable" => quoted_printable(body, false),
        _ => body.to_vec(),
    }
}

/// Base64 that skips line breaks and anything else that isn't part of it, and stops at the padding.
fn base64_lenient(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in text {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Quoted-printable ("=D0=9F", "=" at a line's end joins it to the next); in a header's encoded word "_" is a space.
fn quoted_printable(text: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        if c == b'=' {
            if text.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if text.get(i + 1) == Some(&b'\r') && text.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if let Some(v) = text.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if header && c == b'_' { b' ' } else { c });
        i += 1;
    }
    out
}

/// RFC 2047 encoded words in a header ("=?UTF-8?B?0J/RgNC40LLQtdGC?=" is "Привет"); whitespace between two of them
/// goes.
pub fn decode_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, from) = rest.split_at(start);
        let decoded = encoded_word(from);
        if !(last_was_word && before.trim().is_empty() && decoded.is_some()) {
            out.push_str(before);
        }
        match decoded {
            Some((text, used)) => {
                out.push_str(&text);
                rest = &from[used..];
                last_was_word = true;
            }
            None => {
                out.push_str("=?");
                rest = &from[2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One encoded word at the start of `s`: its text and how many bytes of `s` it took.
fn encoded_word(s: &str) -> Option<(String, usize)> {
    let inner = s.strip_prefix("=?")?;
    let q1 = inner.find('?')?;
    let charset = &inner[..q1];
    let rest = &inner[q1 + 1..];
    let q2 = rest.find('?')?;
    let encoding = &rest[..q2];
    let tail = &rest[q2 + 1..];
    let end = tail.find("?=")?;
    let data = &tail[..end];
    // An encoded word has no spaces in it: "=?" in ordinary text isn't one.
    if charset.is_empty() || charset.contains(char::is_whitespace) || data.contains(char::is_whitespace) {
        return None;
    }
    let bytes = match encoding {
        "B" | "b" => base64_lenient(data.as_bytes()),
        "Q" | "q" => quoted_printable(data.as_bytes(), true),
        _ => return None,
    };
    // A language tag may follow the charset: "utf-8*en".
    let charset = charset.split('*').next().unwrap_or(charset);
    Some((charset_decode(&bytes, Some(charset)), 2 + q1 + 1 + q2 + 1 + end + 2))
}

/// Bytes in a named charset as text; UTF-8 when it isn't named, and the ANSI code page when that fails.
fn charset_decode(bytes: &[u8], charset: Option<&str>) -> String {
    let Some(cs) = charset.map(|c| lower_str(c.trim().trim_matches('"'))) else { return decode(bytes) };
    let code_page = match cs.as_str() {
        "utf-8" | "utf8" => return String::from_utf8_lossy(bytes).into_owned(),
        "us-ascii" | "ascii" => return decode(bytes),
        "koi8-r" => 20866,
        "koi8-u" => 21866,
        "ibm866" | "cp866" => 866,
        "iso-8859-1" | "latin1" => 28591,
        "gb2312" | "gbk" | "gb18030" => 936,
        "big5" => 950,
        "shift_jis" | "shift-jis" | "sjis" => 932,
        "euc-jp" => 51932,
        "iso-2022-jp" => 50220,
        "euc-kr" | "ks_c_5601-1987" => 949,
        c => {
            if let Some(n) = c.strip_prefix("windows-").or_else(|| c.strip_prefix("cp")).and_then(|n| n.parse::<u32>().ok()) {
                n
            } else if let Some(n) = c.strip_prefix("iso-8859-").and_then(|n| n.parse::<u32>().ok()) {
                28590 + n
            } else {
                return decode(bytes);
            }
        }
    };
    crate::win::decode_code_page(bytes, code_page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_message_gives_its_headers_and_text() {
        let raw = b"Subject: Lunch\r\nFrom: Ann <ann@example.com>\r\nTo: bob@example.com\r\nX-Mailer: x\r\n\r\nSee you at noon.\r\n";
        let t = eml(raw);
        assert!(t.contains("Subject: Lunch") && t.contains("From: Ann") && t.contains("See you at noon."), "{t}");
        assert!(!t.contains("X-Mailer"));
    }

    #[test]
    fn mime_parts_are_decoded() {
        let raw = concat!(
            "Subject: =?UTF-8?B?0J/RgNC40LLQtdGC?= =?UTF-8?Q?_=D0=BC=D0=B8=D1=80?=\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
            "\r\n",
            "preamble\r\n",
            "--outer\r\n",
            "Content-Type: multipart/alternative; boundary=inner\r\n",
            "\r\n",
            "--inner\r\n",
            "Content-Type: text/plain; charset=windows-1251\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "=C4=EE=E3=EE=E2=EE=F0 =E0=F0=E5=ED=E4=FB =\r\n",
            "attached\r\n",
            "--inner\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "PHA+SFRNTCB2ZXJzaW9uPC9wPg==\r\n",
            "--inner--\r\n",
            "--outer\r\n",
            "Content-Type: application/pdf; name=\"lease.pdf\"\r\n",
            "Content-Disposition: attachment; filename=\"lease.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "JVBERi0xLjQK\r\n",
            "--outer--\r\n",
        );
        let t = eml(raw.as_bytes());
        assert!(t.contains("Subject: Привет мир"), "{t}");
        assert!(t.contains("Договор аренды attached"), "{t}");
        assert!(!t.contains("HTML version"), "the plain part stands for the alternatives: {t}");
        assert!(t.contains("Attachments: lease.pdf"), "{t}");
        assert!(!t.contains("JVBER") && !t.contains("preamble"), "{t}");
    }

    #[test]
    fn an_html_only_message_loses_its_markup() {
        let raw = b"Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nPHA+SFRNTCB2ZXJzaW9uPC9wPg==\r\n";
        assert!(eml(raw).contains("HTML version"));
    }
}
