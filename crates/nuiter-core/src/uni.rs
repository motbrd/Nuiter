//! Character classes as .NET defines them (Unicode general categories), so word splitting and matching behave as they
//! did in the .NET version, and as SQLite's unicode61 tokenizer sees words.

use unicode_general_category::{GeneralCategory as G, get_general_category};

/// L* (letters).
pub fn is_letter(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    matches!(
        get_general_category(c),
        G::UppercaseLetter | G::LowercaseLetter | G::TitlecaseLetter | G::ModifierLetter | G::OtherLetter
    )
}

/// Nd (decimal digits), as .NET's `char.IsDigit`.
pub fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    get_general_category(c) == G::DecimalNumber
}

pub fn is_letter_or_digit(c: char) -> bool {
    is_letter(c) || is_digit(c)
}

/// Mn, Mc, Me: vowel signs, harakat and other combining marks.
pub fn is_mark(c: char) -> bool {
    !c.is_ascii() && matches!(get_general_category(c), G::NonspacingMark | G::SpacingMark | G::EnclosingMark)
}

pub fn is_nonspacing_mark(c: char) -> bool {
    !c.is_ascii() && get_general_category(c) == G::NonspacingMark
}

/// Lu.
pub fn is_upper(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_uppercase();
    }
    get_general_category(c) == G::UppercaseLetter
}

/// Ll.
pub fn is_lower(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_lowercase();
    }
    get_general_category(c) == G::LowercaseLetter
}

/// Cc.
pub fn is_control(c: char) -> bool {
    c.is_control()
}

/// One character's lowercase, always one character (like .NET's invariant mapping), so positions stay put.
pub fn lower(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// A string's lowercase, character for character.
pub fn lower_str(s: &str) -> String {
    s.chars().map(lower).collect()
}

/// Case-insensitive equality of two strings (character by character, as Windows compares paths).
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars().map(lower).eq(b.chars().map(lower))
}

/// Whether `s` starts with `prefix`, ignoring case.
pub fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    strip_prefix_ignore_case(s, prefix).is_some()
}

/// What follows `prefix` in `s`, ignoring case. Taken by characters, not by `prefix.len()`: a letter and its other case
/// can differ in length ("K", the Kelvin sign, is 3 bytes; "k" is 1), so a byte offset from the prefix could fall
/// inside a character or past the end.
pub fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let mut a = s.char_indices();
    let mut end = 0;
    for p in prefix.chars() {
        match a.next() {
            Some((i, c)) if lower(c) == lower(p) => end = i + c.len_utf8(),
            _ => return None,
        }
    }
    Some(&s[end..])
}

/// Whether `s` ends with `suffix`, ignoring case.
pub fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    let a: Vec<char> = s.chars().collect();
    let b: Vec<char> = suffix.chars().collect();
    b.len() <= a.len() && a[a.len() - b.len()..].iter().zip(&b).all(|(x, y)| lower(*x) == lower(*y))
}

/// Whether `s` contains `part`, ignoring case.
pub fn contains_ignore_case(s: &str, part: &str) -> bool {
    lower_str(s).contains(&lower_str(part))
}

/// Index (in chars) of `needle` in `hay` (both char slices), from `from`.
pub fn find_chars(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= hay.len()).then_some(from);
    }
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}
