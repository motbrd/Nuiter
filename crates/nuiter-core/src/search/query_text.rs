//! Turns a query into keyword-search terms. Words are OR-ed (a file needn't contain every word; coverage is scored
//! instead), stopwords and "kind" words ("photo", "видео") are dropped, and words of languages that inflect (Russian
//! above all) match by their stem, so "кошка" also finds "кошки", "кошкой", "кошек".

use std::collections::HashSet;
use std::sync::LazyLock;

use unicode_normalization::UnicodeNormalization;

use crate::file_kinds::FileCategory;
use crate::uni::{is_digit, is_letter, is_letter_or_digit, is_mark, is_nonspacing_mark, is_upper, lower};

/// A query word for keyword search: what FTS5 matches (`stem`, as a prefix or whole word).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordTerm {
    pub word: String,
    pub stem: String,
    pub prefix: bool,
    pub is_code: bool,
}

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // English
        "a", "an", "the", "of", "in", "on", "at", "for", "to", "and", "or", "is", "are", "was", "were", "be", "with", "by",
        "from", "my", "me", "i", "it", "its", "this", "that", "these", "those", "how", "what", "where", "when", "which",
        "who", "about", "some", "any", "find", "show", "file", "files",
        // Russian
        "и", "в", "во", "на", "по", "с", "со", "к", "ко", "о", "об", "обо", "у", "за", "из", "от", "до", "для", "как",
        "что", "это", "этот", "эта", "эти", "мой", "моя", "моё", "мое", "мои", "где", "когда", "а", "но", "или", "не",
        "ли", "же", "бы", "про", "все", "всё", "найди", "найти", "покажи", "файл", "файлы",
    ]
    .into_iter()
    .collect()
});

/// Words that say what kind of file to look for, not what's in it.
static KIND_NOUNS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "photo", "photos", "picture", "pictures", "image", "images", "screenshot", "screenshots", "video", "videos",
        "recording", "pdf", "pdfs", "document", "documents", "фото", "фотка", "фотки", "фотография", "фотографии",
        "картинка", "картинки", "изображение", "скриншот", "скриншоты", "скрин", "видео", "запись", "документ",
        "документы",
    ]
    .into_iter()
    .collect()
});

/// Words that ask for a kind of file ("…in a video", "скриншот с…"): a soft boost for that kind, not a filter. Cyrillic
/// keys also match as the start of a longer word ("видеозаписи", "скриншоте", "песню").
const HINT_WORDS: &[(&str, FileCategory)] = &[
    ("photo", FileCategory::Image),
    ("photos", FileCategory::Image),
    ("picture", FileCategory::Image),
    ("pictures", FileCategory::Image),
    ("image", FileCategory::Image),
    ("images", FileCategory::Image),
    ("screenshot", FileCategory::Image),
    ("screenshots", FileCategory::Image),
    ("pdf", FileCategory::Pdf),
    ("pdfs", FileCategory::Pdf),
    ("paper", FileCategory::Pdf),
    ("papers", FileCategory::Pdf),
    ("document", FileCategory::Document),
    ("documents", FileCategory::Document),
    ("note", FileCategory::Document),
    ("notes", FileCategory::Document),
    ("video", FileCategory::Video),
    ("videos", FileCategory::Video),
    ("clip", FileCategory::Video),
    ("movie", FileCategory::Video),
    ("audio", FileCategory::Audio),
    ("song", FileCategory::Audio),
    ("podcast", FileCategory::Audio),
    ("voice", FileCategory::Audio),
    ("sound", FileCategory::Audio),
    ("recording", FileCategory::Audio),
    ("фото", FileCategory::Image),
    ("фотк", FileCategory::Image),
    ("картинк", FileCategory::Image),
    ("изображени", FileCategory::Image),
    ("скрин", FileCategory::Image),
    ("снимок", FileCategory::Image),
    ("снимк", FileCategory::Image),
    ("документ", FileCategory::Document),
    ("заметк", FileCategory::Document),
    ("видео", FileCategory::Video),
    ("ролик", FileCategory::Video),
    ("фильм", FileCategory::Video),
    ("аудио", FileCategory::Audio),
    ("песн", FileCategory::Audio),
    ("подкаст", FileCategory::Audio),
    ("голосов", FileCategory::Audio),
];

/// The kinds of file the query asks for.
pub fn asked_kinds(query: &str) -> HashSet<FileCategory> {
    let mut hints = HashSet::new();
    for raw in query.split(' ').filter(|w| !w.is_empty()) {
        let w = normalize(&raw.chars().filter(|&c| is_letter_or_digit(c)).collect::<String>());
        if w.is_empty() {
            continue;
        }
        for (word, category) in HINT_WORDS {
            if w == *word || (is_cyrillic(word) && word.chars().count() >= 4 && w.starts_with(word)) {
                hints.insert(*category);
            }
        }
    }
    hints
}

/// Query words for keyword search. `last_is_typing`: the last word is still being typed, so it also matches as the
/// start of longer words (the search box's instant results); a finished query matches it whole.
pub fn terms(query: &str, last_is_typing: bool) -> Vec<KeywordTerm> {
    let mut words: Vec<String> = vec![];
    let mut sb = String::new();
    for ch in query.chars() {
        if is_letter_or_digit(ch) || ch == '_' || is_mark(ch) {
            sb.push(ch);
        } else if !sb.is_empty() {
            words.push(std::mem::take(&mut sb));
        }
    }
    if !sb.is_empty() {
        words.push(sb);
    }

    let mut terms = vec![];
    for (i, word) in words.iter().enumerate() {
        if terms.len() >= 12 {
            break;
        }
        let w = normalize(word);
        let is_code = w.chars().any(is_digit) || w.contains('_');
        if !is_code && (STOPWORDS.contains(w.as_str()) || KIND_NOUNS.contains(w.as_str())) {
            continue;
        }
        let last = last_is_typing && i == words.len() - 1;
        terms.push(make_term(w, is_code, last));
    }
    // A query of only stop/kind words ("фото") still searches for them literally.
    if terms.is_empty() && !words.is_empty() {
        terms.extend(words.iter().take(3).map(|w| make_term(normalize(w), false, false)));
    }
    terms
}

fn make_term(w: String, is_code: bool, typing: bool) -> KeywordTerm {
    let len = w.chars().count();
    if is_code {
        return KeywordTerm { stem: w.clone(), word: w, prefix: typing && len >= 2, is_code: true };
    }
    if is_cyrillic(&w) {
        // Russian (and other Cyrillic languages) change word endings: match the stem.
        let stem = russian_stem(&w);
        let prefix = stem.chars().count() < len || typing;
        return KeywordTerm { word: w, stem, prefix, is_code: false };
    }
    // English plurals ("invoices" → invoice*), and the word being typed.
    if len >= 5 && w.ends_with('s') && !w.ends_with("ss") {
        let stem = w[..w.len() - 1].to_string();
        return KeywordTerm { word: w, stem, prefix: true, is_code: false };
    }
    KeywordTerm { stem: w.clone(), word: w, prefix: typing && len >= 2, is_code: false }
}

/// Common noun/adjective/verb endings, longest first. A light stemmer: enough for "кошка" to find "кошки", "кошкой",
/// "кошке", and "договор аренды" to find "договору аренде"; the meaning search covers the rest.
const RUSSIAN_ENDINGS: &[&str] = &[
    "ями", "ами", "ого", "его", "ому", "ему", "ыми", "ими", "ется", "ются", "ться", "ешь", "ете", "ишь", "ите", "ах", "ях",
    "ам", "ям", "ом", "ем", "ой", "ей", "ий", "ый", "ая", "яя", "ое", "ее", "ие", "ые", "ую", "юю", "ов", "ев", "ых", "их",
    "ть", "ет", "ит", "ут", "ют", "ат", "ят", "а", "я", "о", "е", "и", "ы", "у", "ю", "ь", "й",
];

fn russian_stem(w: &str) -> String {
    let len = w.chars().count();
    for e in RUSSIAN_ENDINGS {
        let el = e.chars().count();
        if len >= el + 3 && w.ends_with(e) {
            return w[..w.len() - e.len()].to_string();
        }
    }
    w.to_string()
}

fn quoted(t: &KeywordTerm) -> String {
    if t.prefix { format!("\"{}\"*", t.stem) } else { format!("\"{}\"", t.stem) }
}

/// The FTS5 MATCH expression: every term, OR-ed.
pub fn match_expr(terms: &[KeywordTerm]) -> Option<String> {
    (!terms.is_empty()).then(|| terms.iter().map(quoted).collect::<Vec<_>>().join(" OR "))
}

/// The FTS5 expression for the terms side by side, in order ("a"* + "b": a phrase whose parts may be prefixes).
pub fn adjacent(terms: &[KeywordTerm]) -> String {
    terms.iter().map(quoted).collect::<Vec<_>>().join(" + ")
}

/// Share of the terms found in `text` (case-, accent- and ё-insensitive).
pub fn word_share(terms: &[KeywordTerm], text: &str) -> f64 {
    if terms.is_empty() || text.is_empty() {
        return 0.0;
    }
    let t: Vec<char> = normalize(text).chars().collect();
    let hits = terms.iter().filter(|term| contains(&t, term)).count();
    hits as f64 / terms.len() as f64
}

fn contains(text: &[char], term: &KeywordTerm) -> bool {
    let stem: Vec<char> = term.stem.chars().collect();
    let mut i = 0;
    while let Some(at) = crate::uni::find_chars(text, &stem, i) {
        // The match must start a word; and unless it's a prefix term, also end one.
        let starts = at == 0 || !is_letter_or_digit(text[at - 1]);
        let end = at + stem.len();
        let ends = end >= text.len() || !is_letter_or_digit(text[end]);
        if starts && (term.prefix || ends) {
            return true;
        }
        i = end.max(at + 1);
    }
    false
}

/// True when the query is something to look up rather than describe: every word of it is an identifier. That is a word
/// mixing letters and digits ("A-1132", "INV-2025-0345", "B12"), a number of four digits or more (an order, a phone),
/// or parts joined by - _ . / of which one starts with a capital ("Garden-Guest"). "well-known" or "iPhone 15 photos"
/// don't make one.
pub fn is_identifier_query(query: &str) -> bool {
    let words: Vec<&str> = query
        .split(' ')
        .map(|w| w.trim_matches(['.', ',', ';', ':', '!', '?', '"', '\'', '(', ')']))
        .filter(|w| !w.is_empty())
        .collect();
    !words.is_empty() && words.iter().all(|w| is_identifier_word(w))
}

fn is_identifier_word(w: &str) -> bool {
    let digits = w.chars().filter(|&c| is_digit(c)).count();
    if digits > 0 && w.chars().any(is_letter) {
        return true;
    }
    if digits >= 4 && digits == w.chars().filter(|&c| is_letter_or_digit(c)).count() {
        return true;
    }
    let parts: Vec<&str> = w.split(['-', '_', '.', '/']).filter(|p| !p.is_empty()).collect();
    parts.len() >= 2 && parts.iter().any(|p| p.chars().next().is_some_and(is_upper))
}

/// Lowercase, ё as е, and Latin letters without their accents (as FTS5's unicode61 tokenizer folds them); Cyrillic й
/// stays й.
pub fn normalize(s: &str) -> String {
    let lower: String = s.chars().map(|c| if c == 'ё' || c == 'Ё' { 'е' } else { lower(c) }).collect();
    if lower.is_ascii() {
        return lower;
    }
    let mut sb = String::with_capacity(lower.len());
    let mut prev: Option<char> = None;
    for ch in lower.nfd() {
        if is_nonspacing_mark(ch) && prev.is_some_and(|p| p < 'ɐ') {
            continue;
        }
        sb.push(ch);
        prev = Some(ch);
    }
    sb.nfc().collect()
}

fn is_cyrillic(w: &str) -> bool {
    w.chars().any(|c| ('Ѐ'..='ӿ').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_terms_drop_stopwords_and_stem_russian() {
        let t = terms("фото где кошка спит на диване", false);
        assert_eq!(t.iter().map(|x| x.word.as_str()).collect::<Vec<_>>(), vec!["кошка", "спит", "диване"]);
        assert_eq!(match_expr(&t).unwrap(), "\"кошк\"* OR \"спит\" OR \"диван\"*");
        assert_eq!(word_share(&terms("кошка", false), "две кошки на окне"), 1.0);
        assert_eq!(word_share(&terms("кот", false), "который"), 0.0);
        assert!(match_expr(&terms("  -- \" ' ", false)).is_none());
        assert!(is_identifier_query("INV-2026-0042"));
        assert!(is_identifier_query("A-1132 20481632"));
        assert!(is_identifier_query("Garden-Guest"));
        assert!(!is_identifier_query("well-known places"));
        assert!(!is_identifier_query("iPhone 15 photos"));
        assert!(!is_identifier_query("photos from 2026"));
        assert_eq!(adjacent(&terms("Garden-Guest", false)), "\"garden\" + \"guest\"");
        assert_eq!(match_expr(&terms("invoices", false)).unwrap(), "\"invoice\"*");
    }

    #[test]
    fn kind_words_hint_at_a_kind_of_file() {
        assert_eq!(asked_kinds("zebra in a video"), [FileCategory::Video].into());
        assert_eq!(asked_kinds("скриншоте с ошибкой"), [FileCategory::Image].into());
        assert!(asked_kinds("договор аренды").is_empty());
    }

    #[test]
    fn normalizing_folds_case_accents_and_yo() {
        assert_eq!(normalize("Ёлка Café"), "елка cafe");
        assert_eq!(normalize("йод"), "йод");
    }
}
