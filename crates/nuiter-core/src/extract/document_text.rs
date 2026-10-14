//! Plain text of documents: (section label, text) pairs, e.g. one per slide.

use std::fs::File;
use std::io::{BufReader, Read};
use std::sync::LazyLock;

use anyhow::Result;
use quick_xml::events::Event;
use regex::Regex;

use crate::file_kinds::pathx;
use crate::uni::lower_str;

const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

pub fn extract(path: &str) -> Result<Vec<(Option<String>, String)>> {
    let ext = lower_str(pathx::extension(path));
    Ok(match ext.as_str() {
        ".docx" => vec![(None, docx(path)?)],
        ".pptx" => pptx(path)?,
        ".xlsx" => vec![(None, xlsx(path)?)],
        ".odt" | ".odp" | ".ods" => vec![(None, odf(path)?)],
        ".epub" => epub(path)?,
        ".html" | ".htm" => vec![(None, strip_html(&read_text(path)?))],
        ".xml" => vec![(None, strip_xml(&read_text(path)?))],
        ".rtf" => vec![(None, rtf(&read_text(path)?))],
        ".ipynb" => vec![(None, notebook(path)?)],
        ".eml" => vec![(None, super::email::eml(&read_bytes(path)?))],
        ".srt" | ".vtt" => vec![(None, subtitles(&read_text(path)?))],
        ".doc" | ".msg" => vec![(None, super::ifilter::extract(path, 60_000).unwrap_or_default())],
        _ => vec![(None, read_text(path)?)],
    })
}

/// Reads a text file (up to 32 MB), detecting UTF-8/UTF-16 BOMs and falling back to the system ANSI code page.
pub fn read_text(path: &str) -> Result<String> {
    let f = std::fs::OpenOptions::new().read(true).share_mode_all().open(path)?;
    let mut bytes = vec![];
    f.take(MAX_TEXT_BYTES).read_to_end(&mut bytes)?;
    if bytes.len() as u64 == MAX_TEXT_BYTES {
        cut_partial_utf8(&mut bytes);
    }
    Ok(decode(&bytes))
}

/// A file read only up to a limit can end inside a UTF-8 character: those bytes go, or the whole text would fail as
/// UTF-8 and be read in the ANSI code page ("привет" as "Ð¿Ñ€Ð¸Ð²ÐµÑ‚").
fn cut_partial_utf8(bytes: &mut Vec<u8>) {
    if let Err(e) = std::str::from_utf8(bytes)
        && e.error_len().is_none()
    {
        bytes.truncate(e.valid_up_to());
    }
}

trait ShareAll {
    fn share_mode_all(&mut self) -> &mut Self;
}

impl ShareAll for std::fs::OpenOptions {
    /// Others may go on writing, renaming or deleting the file while it's read (FILE_SHARE_READ | WRITE | DELETE).
    fn share_mode_all(&mut self) -> &mut Self {
        use std::os::windows::fs::OpenOptionsExt;
        self.share_mode(0x1 | 0x2 | 0x4)
    }
}

/// Opens a file for reading without stopping others from writing, renaming or deleting it.
pub fn open_shared(path: &str) -> std::io::Result<File> {
    std::fs::OpenOptions::new().read(true).share_mode_all().open(path)
}

pub fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => crate::win::decode_code_page(bytes, crate::win::ansi_code_page()),
    }
}

// ---------- zip containers (OOXML, ODF, EPUB) ----------

struct Container {
    zip: zip::ZipArchive<BufReader<File>>,
    /// Entry names with "/" separators (some writers, e.g. .NET Framework, store "word\document.xml").
    names: Vec<String>,
}

impl Container {
    fn open(path: &str) -> Result<Self> {
        let zip = zip::ZipArchive::new(BufReader::new(open_shared(path)?))?;
        let names = (0..zip.len())
            .map(|i| zip.name_for_index(i).map(|n| n.replace('\\', "/")).unwrap_or_default())
            .collect();
        Ok(Self { zip, names })
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// An entry's bytes (at most 64 MB: a zip bomb gets no further).
    fn read(&mut self, index: usize) -> Result<Vec<u8>> {
        let entry = self.zip.by_index(index)?;
        let mut bytes = vec![];
        entry.take(64 << 20).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// XML as a flat stream: element starts (with local name and the attributes asked for), ends, and text (entities
/// resolved). An empty element `<x/>` comes as a start only.
enum Node<'a> {
    Start { local: &'a str, prefix: &'a str, attrs: &'a quick_xml::events::BytesStart<'a>, empty: bool },
    End { local: &'a str },
    Text(&'a str),
}

fn walk_xml(bytes: &[u8], mut f: impl FnMut(Node<'_>)) -> Result<()> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buf = vec![];
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                let empty = matches!(ev, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = ev else { unreachable!() };
                let name: &str = e.name().0;
                let (prefix, local) = match name.find(':') {
                    Some(i) => (&name[..i], &name[i + 1..]),
                    None => ("", name),
                };
                f(Node::Start { local, prefix, attrs: &e, empty });
            }
            Ok(Event::End(e)) => {
                let name: &str = &e;
                let local = name.rsplit(':').next().unwrap_or(name);
                f(Node::End { local });
            }
            Ok(Event::Text(t)) => f(Node::Text(&t.xml10_content())),
            Ok(Event::CData(t)) => f(Node::Text(&t.xml10_content())),
            Ok(Event::GeneralRef(r)) => {
                if let Some(c) = resolve_ref(&r) {
                    let mut tmp = [0u8; 4];
                    f(Node::Text(c.encode_utf8(&mut tmp)));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            // A broken document: what was read so far counts.
            Err(_) => break,
        }
        buf.clear();
    }
    Ok(())
}

fn resolve_ref(r: &quick_xml::events::BytesRef<'_>) -> Option<char> {
    if r.is_char_ref() {
        return r.resolve_char_ref().ok().flatten();
    }
    match &**r {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => None,
    }
}

#[allow(deprecated)] // normalized_value() also folds whitespace in values, which names keep
fn attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes().flatten().find(|a| {
        let k: &str = a.key.as_ref();
        k == name || k.rsplit(':').next() == Some(name)
    })
    .map(|a| a.unescape_value().map(|v| v.into_owned()).unwrap_or_else(|_| a.value.to_string()))
}

fn docx(path: &str) -> Result<String> {
    let mut zip = Container::open(path)?;
    let mut sb = String::new();
    for name in ["word/document.xml", "word/footnotes.xml", "word/endnotes.xml", "word/comments.xml"] {
        if let Some(i) = zip.index_of(name) {
            append_word_xml(&zip.read(i)?, &mut sb)?;
        }
    }
    let parts: Vec<usize> =
        (0..zip.names.len()).filter(|&i| zip.names[i].starts_with("word/header") || zip.names[i].starts_with("word/footer")).collect();
    for i in parts {
        append_word_xml(&zip.read(i)?, &mut sb)?;
    }
    Ok(sb)
}

fn append_word_xml(xml: &[u8], sb: &mut String) -> Result<()> {
    let mut in_t = 0usize;
    walk_xml(xml, |n| match n {
        // <w:t/> holds nothing.
        Node::Start { local: "t", prefix: "w", empty: false, .. } => in_t += 1,
        Node::Start { local: "tab", .. } => sb.push(' '),
        Node::Start { local: "br" | "cr", .. } => sb.push('\n'),
        Node::End { local: "t" } if in_t > 0 => in_t -= 1,
        Node::End { local: "p" } => sb.push('\n'),
        Node::End { local: "tc" } => sb.push_str(" | "),
        Node::Text(t) if in_t > 0 => sb.push_str(t),
        _ => {}
    })
}

static SLIDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ppt/slides/slide(\d+)\.xml$").unwrap());

fn pptx(path: &str) -> Result<Vec<(Option<String>, String)>> {
    let mut zip = Container::open(path)?;
    let mut slides: Vec<(u32, usize)> = zip
        .names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| SLIDE.captures(n).and_then(|c| c[1].parse().ok()).map(|k| (k, i)))
        .collect();
    slides.sort();
    let mut result = vec![];
    for (k, i) in slides {
        let mut text = drawing_text(&zip.read(i)?)?;
        if let Some(notes) = zip.index_of(&format!("ppt/notesSlides/notesSlide{k}.xml")) {
            text.push('\n');
            text.push_str(&drawing_text(&zip.read(notes)?)?);
        }
        if !text.trim().is_empty() {
            result.push((Some(format!("slide {k}")), text));
        }
    }
    Ok(result)
}

fn drawing_text(xml: &[u8]) -> Result<String> {
    let mut sb = String::new();
    let mut in_t = 0usize;
    walk_xml(xml, |n| match n {
        Node::Start { local: "t", empty: false, .. } => in_t += 1,
        Node::End { local: "t" } if in_t > 0 => in_t -= 1,
        Node::End { local: "p" } => sb.push('\n'),
        Node::Text(t) if in_t > 0 => sb.push_str(t),
        _ => {}
    })?;
    Ok(sb)
}

fn xlsx(path: &str) -> Result<String> {
    let mut zip = Container::open(path)?;
    let mut sb = String::new();
    if let Some(i) = zip.index_of("xl/workbook.xml") {
        walk_xml(&zip.read(i)?, |n| {
            if let Node::Start { local: "sheet", attrs, .. } = n
                && let Some(name) = attr(attrs, "name")
            {
                sb.push_str("Sheet: ");
                sb.push_str(&name);
                sb.push('\n');
            }
        })?;
    }
    if let Some(i) = zip.index_of("xl/sharedStrings.xml") {
        let mut in_t = 0usize;
        walk_xml(&zip.read(i)?, |n| match n {
            Node::Start { local: "t", empty: false, .. } => in_t += 1,
            Node::End { local: "t" } if in_t > 0 => in_t -= 1,
            Node::End { local: "si" } => sb.push('\n'),
            Node::Text(t) if in_t > 0 => sb.push_str(t),
            _ => {}
        })?;
    }
    // Inline strings and numbers from the first sheets (bounded).
    let sheets: Vec<usize> = (0..zip.names.len()).filter(|&i| zip.names[i].starts_with("xl/worksheets/sheet")).take(10).collect();
    for i in sheets {
        let xml = zip.read(i)?;
        let mut cells = 0usize;
        let mut shared_ref = false;
        // The value element being read: "t" (text) or "v" (a value), and what it holds so far.
        let mut value: Option<(bool, String)> = None;
        walk_xml(&xml, |n| {
            if cells >= 20000 {
                return;
            }
            match n {
                Node::Start { local: "c", attrs, .. } => shared_ref = attr(attrs, "t").as_deref() == Some("s"),
                Node::Start { local: l @ ("t" | "v"), empty, .. } => {
                    if empty {
                        // <v/>: an empty value.
                        if l == "t" {
                            sb.push(' ');
                            cells += 1;
                        }
                    } else {
                        value = Some((l == "t", String::new()));
                    }
                }
                Node::Text(t) => {
                    if let Some((_, v)) = &mut value {
                        v.push_str(t);
                    }
                }
                Node::End { local: "t" | "v" } => {
                    if let Some((is_text, v)) = value.take()
                        && (is_text || (!shared_ref && v.chars().count() < 30))
                    {
                        sb.push_str(&v);
                        sb.push(' ');
                        cells += 1;
                    }
                }
                Node::End { local: "row" } => sb.push('\n'),
                _ => {}
            }
        })?;
    }
    Ok(sb)
}

fn odf(path: &str) -> Result<String> {
    let mut zip = Container::open(path)?;
    let Some(i) = zip.index_of("content.xml") else { return Ok(String::new()) };
    let mut sb = String::new();
    walk_xml(&zip.read(i)?, |n| match n {
        // Whitespace between elements isn't text (only xml:space="preserve" would make it so).
        Node::Text(t) if !t.trim().is_empty() => sb.push_str(t),
        Node::Start { local: "s" | "tab", .. } => sb.push(' '),
        Node::Start { local: "line-break", .. } => sb.push('\n'),
        Node::End { local: "p" | "h" } => sb.push('\n'),
        _ => {}
    })?;
    Ok(sb)
}

fn epub(path: &str) -> Result<Vec<(Option<String>, String)>> {
    let mut zip = Container::open(path)?;
    let mut pages: Vec<(String, usize)> = (0..zip.names.len())
        .filter(|&i| {
            let n = lower_str(&zip.names[i]);
            n.ends_with(".xhtml") || n.ends_with(".html")
        })
        .map(|i| (zip.zip.name_for_index(i).map(|n| n.to_string()).unwrap_or_default(), i))
        .collect();
    pages.sort();
    let mut result = vec![];
    for (k, (_, i)) in pages.into_iter().enumerate() {
        let text = strip_html(&String::from_utf8_lossy(&zip.read(i)?));
        if !text.trim().is_empty() {
            result.push((Some(format!("section {}", k + 1)), text));
        }
    }
    Ok(result)
}

// ---------- markup ----------

static SCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap());
static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap());
static NOSCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>").unwrap());
static BLOCK_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<(br|/p|/div|/h[1-6]|/li|/tr|/section|/article)[^>]*>").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

pub fn strip_html(html: &str) -> String {
    let s = SCRIPT.replace_all(html, " ");
    let s = STYLE.replace_all(&s, " ");
    let s = NOSCRIPT.replace_all(&s, " ");
    let s = BLOCK_TAG.replace_all(&s, "\n");
    let s = TAG.replace_all(&s, " ");
    html_decode(&s)
}

fn strip_xml(xml: &str) -> String {
    html_decode(&TAG.replace_all(xml, " "))
}

/// HTML character references: numeric ones and the named ones documents use.
pub fn html_decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(|c: char| c == ';' || c == '&' || c.is_whitespace() || c == '<').map(|e| e + 1);
        if let Some(e) = end
            && rest.as_bytes()[e] == b';'
            && let Some(c) = entity(&rest[1..e])
        {
            out.push(c);
            rest = &rest[e + 1..];
            continue;
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let n = if let Some(hex) = num.strip_prefix(['x', 'X']) { u32::from_str_radix(hex, 16).ok()? } else { num.parse().ok()? };
        return char::from_u32(n);
    }
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "shy" => '\u{ad}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "iexcl" => '¡',
        "iquest" => '¿',
        "agrave" => 'à',
        "aacute" => 'á',
        "acirc" => 'â',
        "atilde" => 'ã',
        "auml" => 'ä',
        "aring" => 'å',
        "aelig" => 'æ',
        "ccedil" => 'ç',
        "egrave" => 'è',
        "eacute" => 'é',
        "ecirc" => 'ê',
        "euml" => 'ë',
        "igrave" => 'ì',
        "iacute" => 'í',
        "icirc" => 'î',
        "iuml" => 'ï',
        "ntilde" => 'ñ',
        "ograve" => 'ò',
        "oacute" => 'ó',
        "ocirc" => 'ô',
        "otilde" => 'õ',
        "ouml" => 'ö',
        "oslash" => 'ø',
        "ugrave" => 'ù',
        "uacute" => 'ú',
        "ucirc" => 'û',
        "uuml" => 'ü',
        "yacute" => 'ý',
        "yuml" => 'ÿ',
        "szlig" => 'ß',
        "Agrave" => 'À',
        "Aacute" => 'Á',
        "Acirc" => 'Â',
        "Atilde" => 'Ã',
        "Auml" => 'Ä',
        "Aring" => 'Å',
        "AElig" => 'Æ',
        "Ccedil" => 'Ç',
        "Egrave" => 'È',
        "Eacute" => 'É',
        "Ecirc" => 'Ê',
        "Euml" => 'Ë',
        "Igrave" => 'Ì',
        "Iacute" => 'Í',
        "Icirc" => 'Î',
        "Iuml" => 'Ï',
        "Ntilde" => 'Ñ',
        "Ograve" => 'Ò',
        "Oacute" => 'Ó',
        "Ocirc" => 'Ô',
        "Otilde" => 'Õ',
        "Ouml" => 'Ö',
        "Oslash" => 'Ø',
        "Ugrave" => 'Ù',
        "Uacute" => 'Ú',
        "Ucirc" => 'Û',
        "Uuml" => 'Ü',
        "Yacute" => 'Ý',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "harr" => '↔',
        "check" => '✓',
        _ => return None,
    };
    Some(c)
}

/// Minimal RTF reader: drops control words and destination groups, decodes \'hh and \uN escapes. A \uN is followed by
/// \ucN characters (1 unless said otherwise) for readers without Unicode, a \'hh counting as one: those are skipped
/// ("舒\'97" is one dash, not two).
pub fn rtf(rtf: &str) -> String {
    let chars: Vec<char> = rtf.chars().collect();
    let mut out: Vec<u16> = vec![];
    // Each open group's (skipping, \uc) to go back to at its end.
    let mut groups: Vec<(bool, usize)> = vec![];
    let mut skipping = false;
    let mut uc = 1usize;
    let mut fallback = 0usize;
    let mut cp = 1252u32;
    let mut ansi: Vec<u8> = vec![];
    let flush = |ansi: &mut Vec<u8>, out: &mut Vec<u16>, cp: u32| {
        if !ansi.is_empty() {
            out.extend(crate::win::decode_code_page(ansi, cp).encode_utf16());
            ansi.clear();
        }
    };
    let push = |out: &mut Vec<u16>, c: char| {
        let mut b = [0u16; 2];
        out.extend_from_slice(c.encode_utf16(&mut b));
    };
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if fallback > 0 {
            // The stand-in for the \uN just read.
            if c == '\\' && chars.get(i + 1) == Some(&'\'') {
                i += 4;
                fallback -= 1;
                continue;
            }
            if c == '\r' || c == '\n' {
                i += 1;
                continue;
            }
            if c != '{' && c != '}' && c != '\\' {
                i += 1;
                fallback -= 1;
                continue;
            }
            fallback = 0;
        }
        if c == '{' {
            groups.push((skipping, uc));
            i += 1;
            continue;
        }
        if c == '}' {
            flush(&mut ansi, &mut out, cp);
            (skipping, uc) = groups.pop().unwrap_or((false, 1));
            i += 1;
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            if n == '\'' && i + 3 < chars.len() {
                if !skipping {
                    let hex: String = chars[i + 2..i + 4].iter().collect();
                    if let Ok(b) = u8::from_str_radix(&hex, 16) {
                        ansi.push(b);
                    }
                }
                i += 4;
                continue;
            }
            flush(&mut ansi, &mut out, cp);
            if matches!(n, '\\' | '{' | '}') {
                if !skipping {
                    push(&mut out, n);
                }
                i += 2;
                continue;
            }
            if n == '*' {
                skipping = true;
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let word: String = chars[i + 1..j].iter().collect();
            let mut k = j;
            if k < chars.len() && (chars[k] == '-' || chars[k].is_ascii_digit()) {
                k += 1;
                while k < chars.len() && chars[k].is_ascii_digit() {
                    k += 1;
                }
            }
            let param: String = chars[j..k].iter().collect();
            if k < chars.len() && chars[k] == ' ' {
                k += 1;
            }
            i = k;
            match word.as_str() {
                "par" | "line" | "sect" | "page" | "row" => {
                    if !skipping {
                        out.push('\n' as u16);
                    }
                }
                "tab" | "cell" => {
                    if !skipping {
                        out.push(' ' as u16);
                    }
                }
                "ansicpg" => {
                    if let Ok(p) = param.parse() {
                        cp = p;
                    }
                }
                "u" => {
                    if !skipping && let Ok(u) = param.parse::<i32>() {
                        out.push((if u < 0 { u + 65536 } else { u }) as u16);
                    }
                    fallback = uc;
                }
                "uc" => uc = param.parse().unwrap_or(1),
                "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict" | "object" | "header" | "footer" | "themedata"
                | "datastore" | "latentstyles" => skipping = true,
                _ => {}
            }
            continue;
        }
        i += 1;
        if c == '\r' || c == '\n' {
            continue;
        }
        flush(&mut ansi, &mut out, cp);
        if !skipping {
            push(&mut out, c);
        }
    }
    flush(&mut ansi, &mut out, cp);
    String::from_utf16_lossy(&out)
}

fn notebook(path: &str) -> Result<String> {
    let json: serde_json::Value = serde_json::from_str(&read_text(path)?)?;
    let mut sb = String::new();
    let Some(cells) = json.get("cells").and_then(|c| c.as_array()) else { return Ok(sb) };
    for cell in cells {
        let Some(src) = cell.get("source") else { continue };
        match src {
            serde_json::Value::Array(a) => a.iter().filter_map(|l| l.as_str()).for_each(|l| sb.push_str(l)),
            serde_json::Value::String(s) => sb.push_str(s),
            _ => {}
        }
        sb.push_str("\n\n");
    }
    Ok(sb)
}

/// A file's bytes as they are (up to 32 MB), for formats that say their own encoding (e-mail).
fn read_bytes(path: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    open_shared(path)?.take(MAX_TEXT_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn subtitles(text: &str) -> String {
    text.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("-->") && l.parse::<i64>().is_err() && *l != "WEBVTT")
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn docx_extracts_paragraph_text() {
        let dir = std::env::temp_dir().join(format!("nuiter-docx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.docx");
        {
            let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
            // A backslash separator, as .NET Framework writes it.
            zip.start_file(r"word\document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:body></w:document>"#).unwrap();
            zip.finish().unwrap();
        }
        let text = extract(path.to_str().unwrap()).unwrap();
        assert_eq!(text[0].1, "Hello world\nSecond\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rtf_is_converted_to_plain_text() {
        let text = rtf(r"{\rtf1\ansi\ansicpg1251{\fonttbl{\f0 Arial;}}\f0 Hello \b bold\b0 world\par \'cf\'f0\'e8\'e2\'e5\'f2 \u1052?ир}");
        assert!(text.contains("Hello bold"), "{text}");
        assert!(text.contains("world"));
        assert!(text.contains("Привет"), "{text}");
        assert!(text.contains("Мир"), "{text:?}");
        assert!(!text.contains("Arial"));
        // Word writes a stand-in after each \uN: it's skipped, as many as \ucN says.
        let bs = char::from(92u8);
        let dash = format!("{{{bs}rtf1{bs}ansi{bs}ansicpg1252{bs}uc1 a{bs}u8212{bs}'97b {bs}uc2 c{bs}u8212{bs}'97{bs}'97d {bs}uc0 e{bs}u8212 f}}");
        assert_eq!(rtf(&dash), "a\u{2014}b c\u{2014}d e\u{2014}f");
    }

    #[test]
    fn html_strips_scripts_and_tags() {
        let text = strip_html("<html><script>var x=1;</script><p>Apple&nbsp;pie</p><div>bake</div></html>");
        assert!(!text.contains("var x"));
        assert!(text.contains("Apple"));
        assert!(text.contains("bake"));
        assert_eq!(html_decode("a &amp; b &#1052; &#x41; &bogus; &"), "a & b М A &bogus; &");
    }

    #[test]
    fn a_text_cut_inside_a_character_stays_utf8() {
        let mut bytes = "привет мир".as_bytes().to_vec();
        bytes.pop();
        cut_partial_utf8(&mut bytes);
        assert_eq!(decode(&bytes), "привет ми");
    }

    #[test]
    fn decode_falls_back_to_ansi_code_page() {
        assert_eq!(decode("привет".as_bytes()), "привет");
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'x']), "x");
    }
}
