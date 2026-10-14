//! Prose into pieces the model reads one at a time. A piece is a run of whole sentences up to `CHUNK_CHARS` characters
//! (~400 tokens); the next piece repeats the last sentences of the one before (up to `CARRY_CHARS`), so a thought that
//! spans the cut is whole in at least one of them. A piece that's mostly full ends early at a paragraph break rather
//! than taking the first sentence of the next paragraph.

use crate::uni::{is_control, is_letter, is_mark};

pub const CHUNK_CHARS: usize = 1600;
pub const CARRY_CHARS: usize = 300;
/// Only machine-made giants get this far (~8 million characters): the rest is left out.
pub const MAX_CHUNKS_PER_DOCUMENT: usize = 5000;

/// A piece this full (share of `CHUNK_CHARS`) ends at the next paragraph break.
const PARAGRAPH_ENDS_AT: f64 = 0.75;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gap {
    None,
    Space,
    Line,
    Paragraph,
}

/// A sentence (or a slice of an endless one) and what separates it from the one before. Lengths are in chars.
struct Unit {
    text: String,
    len: usize,
    before: Gap,
}

pub fn split(text: &str) -> Vec<String> {
    split_with(text, CHUNK_CHARS, CARRY_CHARS)
}

pub fn split_with(text: &str, size: usize, carry: usize) -> Vec<String> {
    let units = units(&normalize(text), size);
    let mut out = vec![];
    if units.is_empty() {
        return out;
    }
    let mut start = 0;
    while start < units.len() {
        // Take whole units while they fit; one always fits (units() cut longer ones).
        let mut end = start;
        let mut length = units[start].len;
        while end + 1 < units.len() {
            let next = &units[end + 1];
            let grown = length + 1 + next.len;
            if grown > size {
                break;
            }
            if next.before == Gap::Paragraph && length as f64 >= PARAGRAPH_ENDS_AT * size as f64 {
                break;
            }
            length = grown;
            end += 1;
        }
        out.push(join(&units, start, end));
        if end + 1 >= units.len() {
            break;
        }
        // The next piece starts with the last sentences of this one, as many as fit in the carry (never all of them).
        let mut next0 = end + 1;
        let mut carried = 0;
        while next0 - 1 > start && carried + units[next0 - 1].len < carry && units[next0].before != Gap::Paragraph {
            carried += units[next0 - 1].len + 1;
            next0 -= 1;
        }
        start = next0;
    }
    out
}

fn join(units: &[Unit], from: usize, to: usize) -> String {
    let mut s = units[from].text.clone();
    for u in &units[from + 1..=to] {
        s.push_str(match u.before {
            Gap::Paragraph => "\n\n",
            Gap::Line => "\n",
            _ => " ",
        });
        s.push_str(&u.text);
    }
    s
}

/// The text's sentences, each with the gap before it. A sentence longer than `size` is cut at spaces (or anywhere, if
/// it has none) into slices that fit.
fn units(text: &str, size: usize) -> Vec<Unit> {
    let mut units: Vec<Unit> = vec![];
    let mut gap = Gap::None;
    for paragraph in text.split("\n\n") {
        for line in paragraph.split('\n') {
            for sentence in sentences(line) {
                for slice in slices(&sentence, size) {
                    let before = if units.is_empty() { Gap::None } else { gap };
                    let len = slice.chars().count();
                    units.push(Unit { text: slice, len, before });
                    gap = Gap::Space;
                }
            }
            gap = Gap::Line;
        }
        gap = Gap::Paragraph;
    }
    units
}

/// Sentences of one line: they end at . ! ? … (or their CJK forms), followed by a space or by the line's end.
fn sentences(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = vec![];
    let mut from = 0;
    for i in 0..chars.len() {
        let c = chars[i];
        let ends = (matches!(c, '.' | '!' | '?' | '…') && (i + 1 == chars.len() || chars[i + 1] == ' '))
            || matches!(c, '。' | '！' | '？');
        if !ends {
            continue;
        }
        // "e.g. " and "т.е. ": a one-letter word before the dot is an abbreviation, not a sentence's end.
        if c == '.' && i >= 1 && is_letter(chars[i - 1]) && (i < 2 || !is_letter(chars[i - 2])) {
            continue;
        }
        let s: String = chars[from..=i].iter().collect::<String>().trim().to_string();
        if !s.is_empty() {
            out.push(s);
        }
        from = i + 1;
    }
    let rest: String = chars[from..].iter().collect::<String>().trim().to_string();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

fn slices(sentence: &str, size: usize) -> Vec<String> {
    let mut out = vec![];
    let mut rest: Vec<char> = sentence.chars().collect();
    while rest.len() > size {
        // The last space at or before `size`.
        let space = rest[..=size.min(rest.len() - 1)].iter().rposition(|&c| c == ' ');
        let cut = match space {
            Some(i) if i >= size / 3 => i,
            _ => size,
        };
        let head: String = rest[..cut].iter().collect();
        out.push(head.trim_end().to_string());
        let tail: String = rest[cut..].iter().collect();
        rest = tail.trim_start().chars().collect();
    }
    if !rest.is_empty() {
        out.push(rest.into_iter().collect());
    }
    out
}

/// Whether text is written in words rather than being a table of numbers or a data dump (which match nothing anyone
/// would describe, so they get keyword search only). A token is wordy when it has two or more letters and at least as
/// many letters as other characters; text has words when wordy tokens are 40% of its tokens or more.
pub fn is_prose(text: &str) -> bool {
    let (mut tokens, mut wordy) = (0usize, 0usize);
    for token in text.split_whitespace() {
        tokens += 1;
        let (mut letters, mut other) = (0usize, 0usize);
        for c in token.chars() {
            if is_letter(c) || is_mark(c) {
                letters += 1;
            } else {
                other += 1;
            }
        }
        if letters >= 2 && letters >= other {
            wordy += 1;
        }
    }
    if tokens < 3 { wordy > 0 } else { wordy * 10 >= tokens * 4 }
}

/// Text with \r gone, tabs as spaces, one space between words, at most one blank line between paragraphs, and no
/// control characters; trimmed.
pub fn normalize(text: &str) -> String {
    let mut sb = String::with_capacity(text.len());
    let mut spaces = 0;
    let mut newlines = 0;
    for raw in text.chars() {
        let mut ch = raw;
        if ch == '\r' {
            continue;
        }
        if ch == '\t' || ch == ' ' {
            ch = ' ';
        }
        if is_control(ch) && ch != '\n' {
            continue;
        }
        if ch == '\n' {
            newlines += 1;
            spaces = 0;
            if newlines <= 2 {
                while sb.ends_with(' ') {
                    sb.pop();
                }
                sb.push('\n');
            }
            continue;
        }
        if ch == ' ' {
            spaces += 1;
            if spaces > 1 || sb.is_empty() || sb.ends_with('\n') {
                continue;
            }
            sb.push(' ');
            continue;
        }
        spaces = 0;
        newlines = 0;
        sb.push(ch);
    }
    sb.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_whole_sentences_and_carries_the_last_ones_over() {
        let text = (1..=200).map(|i| format!("Sentence number {i} says something about item {i}.")).collect::<Vec<_>>().join(" ");
        let chunks = split(&text);
        assert!(chunks.len() > 5);
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_CHARS);
            assert!(!c.contains("  "));
            assert!(c.starts_with("Sentence number"));
            assert!(c.ends_with('.'));
        }
        for i in 1..chunks.len() {
            let prev = &chunks[i - 1];
            let last = &prev[prev.rfind("Sentence number").unwrap()..];
            assert!(chunks[i].contains(last));
            let first = &chunks[i][..chunks[i].find('.').unwrap() + 1];
            assert!(prev.contains(first));
        }
        assert!(chunks.last().unwrap().contains("item 200."));
    }

    #[test]
    fn ends_a_full_piece_at_a_paragraph_break_and_cuts_endless_sentences() {
        let para =
            (1..=26).map(|i| format!("Line {i} of the first part, which keeps going for a while.")).collect::<Vec<_>>().join(" ");
        let chunks = split(&format!("{para}\n\nSecond part starts here. It goes on."));
        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].starts_with("Second part"));

        let endless = vec!["word"; 1000].join(" ");
        let pieces = split(&endless);
        for p in &pieces {
            assert!(p.len() <= CHUNK_CHARS);
            assert!(p.split(' ').all(|w| w == "word"));
        }
        assert_eq!(split("e.g. this stays one sentence."), vec!["e.g. this stays one sentence."]);
    }

    #[test]
    fn keeps_short_text_whole() {
        assert_eq!(split("  Hello   world \r\n"), vec!["Hello world"]);
        assert!(split("   \n ").is_empty());
    }

    #[test]
    fn numeric_text_is_detected() {
        assert!(!is_prose("1,2,3,4\n5,6,7,8\n9,10,11,12\n13,14,15,16\n17,18,19,20"));
        assert!(is_prose("Invoice 2026-0042 for 32 hours of consulting work"));
    }
}
