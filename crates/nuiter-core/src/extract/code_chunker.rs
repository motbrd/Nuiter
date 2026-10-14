//! Code into pieces of whole lines. Where a piece ends is chosen, among the lines that would keep it at least
//! `MIN_FILL` full, by what a cut there costs: cutting before a line costs its indentation depth (a cut at the left
//! margin costs nothing), less after a blank line, more before a closing line or right under a comment or an attribute
//! that belongs to it. A piece that starts inside a block gets the block's opening line (the signature) as a header,
//! so the model knows whose lines these are; its line numbers stay those of the real lines.

use std::sync::LazyLock;

use regex::Regex;

/// A piece of a code file: its text and the lines (or notebook cells) it covers, from 1.
#[derive(Debug, Clone, PartialEq)]
pub struct CodePiece {
    pub text: String,
    pub first: usize,
    pub last: usize,
}

pub const PIECE_CHARS: usize = 1500;
/// The model reads 2,048 tokens of an input at most: a single line longer than this is cut.
pub const MAX_PIECE_CHARS: usize = 4000;
const MIN_FILL: f64 = 0.45;
const MAX_HEADER_CHARS: usize = 200;

/// A line that only closes a block: brackets, or an end keyword (Ruby, Lua, shell, VB, the preprocessor).
static CLOSER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^([\)\]\}]+[;,]?|end\b.*|fi|done|esac|#endif\b.*|#end region\b.*|@end)$").unwrap());
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----").unwrap());
static GENERATED_NOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)auto-?generated|do not (edit|modify)|generated (by|from|with)\b|@generated").unwrap());

/// A comment, doc comment, decorator or attribute line: it belongs to the line below it.
fn is_annotation(t: &str) -> bool {
    if let Some(rest) = t.strip_prefix('#') {
        return !(rest.starts_with("endif") || rest.starts_with("region") || rest.starts_with("end"));
    }
    let mut c = t.chars();
    let (a, b) = (c.next(), c.next());
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("--")
        || (a == Some('@') && b.is_some_and(|b| b.is_ascii_alphabetic()))
        || (a == Some('[') && b.is_some_and(|b| b.is_ascii_uppercase()))
        || t.starts_with("'''")
        || t.starts_with("\"\"\"")
        || t.starts_with("<!--")
}

fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

pub fn split(code: &str) -> Vec<CodePiece> {
    split_with(code, PIECE_CHARS)
}

pub fn split_with(code: &str, size: usize) -> Vec<CodePiece> {
    let normalized = code.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut count = lines.len();
    while count > 0 && is_blank(lines[count - 1]) {
        count -= 1;
    }
    let mut out = vec![];
    if count == 0 {
        return out;
    }
    let lens: Vec<usize> = lines.iter().map(|l| char_len(l)).collect();
    let mut indent = vec![0i64; count];
    let mut blank = vec![false; count];
    let mut unit = 0i64;
    for i in 0..count {
        blank[i] = is_blank(lines[i]);
        indent[i] = if blank[i] { -1 } else { width(lines[i]) };
        if indent[i] > 0 && (unit == 0 || indent[i] < unit) {
            unit = indent[i];
        }
    }
    if unit == 0 {
        unit = 4;
    }
    // A blank line takes the indentation of the next line that isn't blank.
    let mut next = 0;
    for i in (0..count).rev() {
        if blank[i] {
            indent[i] = next;
        } else {
            next = indent[i];
        }
    }

    let cost = |i: usize| -> f64 {
        let mut c = indent[i] as f64 / unit as f64;
        if CLOSER.is_match(lines[i].trim()) {
            c += 4.0;
        }
        if i > 0 && blank[i - 1] {
            c -= 0.75;
        }
        if i > 0 && !blank[i - 1] && indent[i - 1] == indent[i] && is_annotation(lines[i - 1].trim_start()) {
            c += 2.0;
        }
        c
    };

    let mut header: Option<String> = None;
    let mut start = 0;
    while start < count {
        while start < count && blank[start] {
            start += 1;
        }
        if start >= count {
            break;
        }
        let budget = size as i64 - header.as_ref().map_or(0, |h| char_len(h) as i64 + 1);
        let mut end = start + 1; // lines[start..end)
        let mut length = lens[start] as i64;
        while end < count && length + 1 + lens[end] as i64 <= budget {
            length += 1 + lens[end] as i64;
            end += 1;
        }
        if end < count {
            // The cheapest cut in the piece's last part; the later one of equal cost.
            let floor = MIN_FILL * budget as f64;
            let (mut best, mut best_cost) = (end, cost(end));
            let mut up_to = length;
            let mut c = end - 1;
            while c > start {
                up_to -= lens[c] as i64 + 1;
                if (up_to as f64) < floor {
                    break;
                }
                if !blank[c] {
                    let k = cost(c);
                    if k < best_cost {
                        best = c;
                        best_cost = k;
                    }
                }
                c -= 1;
            }
            end = best;
        }
        let mut last = end;
        while last > start + 1 && blank[last - 1] {
            last -= 1;
        }
        let mut body = lines[start..last].join("\n");
        if let Some(h) = &header {
            body = format!("{h}\n{body}");
        }
        if char_len(&body) > MAX_PIECE_CHARS {
            body = truncate_chars(&body, MAX_PIECE_CHARS);
        }
        out.push(CodePiece { text: body, first: start + 1, last });
        header = if end < count { enclosing(&lines, &indent, &blank, end) } else { None };
        start = end;
    }
    out
}

/// The line that opens the block `at` is in: the nearest line above it with less indentation that isn't a bare brace.
fn enclosing(lines: &[&str], indent: &[i64], blank: &[bool], at: usize) -> Option<String> {
    let mut depth = indent[at];
    if depth == 0 {
        return None;
    }
    for i in (0..at).rev() {
        if blank[i] || indent[i] >= depth {
            continue;
        }
        let t = lines[i].trim();
        if matches!(t, "{" | "(" | "[") {
            depth = indent[i] + 1;
            continue;
        }
        return Some(if char_len(t) > MAX_HEADER_CHARS { truncate_chars(t, MAX_HEADER_CHARS) } else { t.to_string() });
    }
    None
}

fn width(line: &str) -> i64 {
    let mut w = 0;
    for c in line.chars() {
        match c {
            ' ' => w += 1,
            '\t' => w += 4,
            _ => break,
        }
    }
    w
}

// ---------- notebooks ----------

/// A notebook cell: "code", "markdown" or "raw", and what was written in it. Outputs are never read.
#[derive(Debug, Clone)]
pub struct Cell {
    pub kind: String,
    pub source: String,
}

pub fn read_notebook(json: &str) -> Vec<Cell> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else { return vec![] };
    let cells = root
        .get("cells")
        .or_else(|| root.get("worksheets").and_then(|w| w.get(0)).and_then(|w| w.get("cells"))) // nbformat 3
        .and_then(|c| c.as_array());
    let Some(cells) = cells else { return vec![] };
    cells
        .iter()
        .map(|cell| {
            let kind = cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("code").to_string();
            let source = cell.get("source").or_else(|| cell.get("input"));
            let text = match source {
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|l| l.as_str()).collect::<String>(),
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            Cell { kind, source: text }
        })
        .collect()
}

/// A notebook's pieces, by cells (numbered from 1). A markdown cell starts a piece and takes the code cells after it,
/// as many as fit: notes describe the code that follows them. A cell too long for a piece is split like any code, each
/// part pointing at that cell. Empty cells are passed over.
pub fn split_notebook(cells: &[Cell]) -> Vec<CodePiece> {
    split_notebook_with(cells, PIECE_CHARS)
}

pub fn split_notebook_with(cells: &[Cell], size: usize) -> Vec<CodePiece> {
    let mut out = vec![];
    let mut text = String::new();
    let mut text_len = 0usize;
    let (mut first, mut last) = (0, 0);
    for (i, cell) in cells.iter().enumerate() {
        let body = cell.source.trim();
        let body_len = char_len(body);
        if body_len == 0 {
            continue;
        }
        let number = i + 1;
        let notes = cell.kind == "markdown";
        if text_len > 0 && (notes || body_len > size || text_len + 2 + body_len > size) {
            out.push(CodePiece { text: std::mem::take(&mut text), first, last });
            text_len = 0;
        }
        if body_len > size {
            for part in split_with(body, size) {
                out.push(CodePiece { text: part.text, first: number, last: number });
            }
            continue;
        }
        if text_len == 0 {
            first = number;
        } else {
            text.push_str("\n\n");
            text_len += 2;
        }
        text.push_str(body);
        text_len += body_len;
        last = number;
    }
    if text_len > 0 {
        out.push(CodePiece { text, first, last });
    }
    out
}

// ---------- what isn't worth reading ----------

/// Why a code file isn't read, or None. Never one that holds a private key. For source code (not a repo's notes): not
/// one its first lines say a tool generated, nor a minified one (most of its characters on lines over 1,000 long).
pub fn why_left_out(text: &str, source: bool) -> Option<&'static str> {
    if PRIVATE_KEY.is_match(text) {
        return Some("contains a private key");
    }
    if !source {
        return None;
    }
    let head = match text.char_indices().nth(4000) {
        Some((i, _)) => &text[..i],
        None => text,
    };
    for (n, l) in head.lines().enumerate() {
        if n >= 25 {
            break;
        }
        let t = l.trim_start();
        if !t.is_empty() && (is_annotation(t) || t.contains("<auto-generated")) && GENERATED_NOTE.is_match(t) {
            return Some("generated by a tool");
        }
    }
    let total = char_len(text);
    if total > 5000 {
        let long: usize = text.lines().map(char_len).filter(|&n| n > 1000).sum();
        if long * 10 >= total * 6 {
            return Some("minified");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_cover_every_line_once_and_report_their_lines() {
        let lines: Vec<String> = (1..=200).map(|i| if i % 30 == 0 { String::new() } else { format!("    line{i}();") }).collect();
        let pieces = split(&lines.join("\n"));
        assert!(pieces.len() >= 2);
        assert_eq!(pieces[0].first, 1);
        for i in 1..pieces.len() {
            assert!(pieces[i].first > pieces[i - 1].last);
        }
        for p in &pieces {
            assert!(p.text.contains(&format!("line{}()", p.last)));
        }
    }

    #[test]
    fn code_is_cut_at_the_left_margin_when_it_can_be() {
        let code = (1..=400)
            .map(|i| {
                if i % 25 == 0 {
                    String::new()
                } else if i % 25 == 1 {
                    format!("def f{i}(x):")
                } else {
                    format!("    value_{i} = compute_something_long(x, {i}) + other_value_{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let pieces = split(&code);
        for p in &pieces {
            assert!(p.text.len() <= PIECE_CHARS);
            assert!(p.text.starts_with("def "));
        }
        let huge = split(&"x".repeat(10_000));
        assert_eq!(huge.len(), 1);
        assert!(huge[0].text.len() <= MAX_PIECE_CHARS);
    }

    #[test]
    fn a_piece_that_starts_inside_a_function_gets_its_signature() {
        let body = (1..=120).map(|i| format!("        total += weight_{i} * measure(sample_{i})")).collect::<Vec<_>>().join("\n");
        let code = format!("class Scale:\n    def weigh(self, sample):\n{body}\n        return total\n");
        let pieces = split(&code);
        assert!(pieces.len() >= 2);
        assert!(pieces[1].text.starts_with("def weigh(self, sample):\n"));
        assert!(!code.split('\n').nth(pieces[1].first - 1).unwrap().contains("def weigh"));
    }

    #[test]
    fn notebooks_are_read_by_cells_notes_with_their_code() {
        let nb = r###"{"cells":[{"cell_type":"markdown","source":["# Plot classes"]},{"cell_type":"code","source":["df.plot()"],"outputs":[{"text":"OUTPUT"}]},{"cell_type":"code","source":[]},{"cell_type":"markdown","source":"## Save"},{"cell_type":"code","source":"df.to_csv('x.csv')"}]}"###;
        let cells = read_notebook(nb);
        assert_eq!(cells.len(), 5);
        let pieces = split_notebook(&cells);
        assert_eq!(pieces.iter().map(|p| (p.first, p.last)).collect::<Vec<_>>(), vec![(1, 2), (4, 5)]);
        assert!(pieces.iter().all(|p| !p.text.contains("OUTPUT")));
    }

    #[test]
    fn generated_code_and_keys_are_left_out() {
        assert!(why_left_out("// Code generated by protoc. DO NOT EDIT.\npackage x", true).is_some());
        assert!(why_left_out("// <auto-generated>\n//   This code was generated by a tool.\nnamespace X;", true).is_some());
        assert!(why_left_out("-----BEGIN RSA PRIVATE KEY-----", false).is_some());
        assert!(why_left_out(&"a".repeat(8000), true).is_some());
        assert!(why_left_out("# Notes\nDO NOT EDIT this list by hand", false).is_none());
        assert!(why_left_out("def f():\n    return 1", true).is_none());
        assert!(why_left_out("x = 1  # do not edit by hand, see below\n", true).is_none());
    }
}
