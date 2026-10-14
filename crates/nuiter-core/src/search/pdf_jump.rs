//! A word to search for when a PDF opens at a matching page (a "landmark"). Viewers that take a search jump to its
//! first match and highlight it; every earlier occurrence is a step the reader has to click past. So of the query's
//! words on that page, the one that occurs least often on the pages before it is chosen.

use super::engine;
use super::query_text::{self, KeywordTerm};
use crate::extract::pdf;
use crate::paths::Log;
use crate::uni::{is_letter_or_digit, is_mark};

/// Earlier pages read to check a word isn't there first; past this the page's own words are trusted.
const MAX_PAGES_BEFORE: usize = 300;

/// The query word on page `page` (1-based) with the fewest occurrences on earlier pages, spelled as the page spells it;
/// of equally good ones, the longer, then the earlier in the query. None when none of them is on it, or the PDF can't
/// be read.
pub fn find(path: &str, page: usize, query: &str) -> Option<String> {
    // Without its operators: "type:pdf" isn't something to look for in the text.
    let terms: Vec<KeywordTerm> =
        query_text::terms(&engine::parse(query, None).text, false).into_iter().filter(|t| t.word.chars().count() >= 3).collect();
    if terms.is_empty() || page < 1 {
        return None;
    }
    let doc = match pdf::open(path) {
        Ok(d) => d,
        Err(e) => {
            Log::warn(format!("No search word for {path} p.{page}: {e}"));
            return None;
        }
    };
    if page > doc.page_count().ok()? {
        return None;
    }
    let first = page.saturating_sub(MAX_PAGES_BEFORE).max(1);
    let before: Vec<String> = (first..page).map(|n| pdf::page_text(&doc, n - 1)).collect();
    find_in(&pdf::page_text(&doc, page - 1), &before, &terms)
}

/// `target`: the page's text; `before`: the earlier pages' texts.
pub fn find_in(target: &str, before: &[String], terms: &[KeywordTerm]) -> Option<String> {
    let mut on_page: Vec<String> = vec![];
    for t in terms {
        if let Some(w) = word_for(t, target)
            && !on_page.contains(&w)
        {
            on_page.push(w);
        }
    }
    if on_page.is_empty() {
        return None;
    }
    let earlier: Vec<Vec<char>> = before.iter().map(|b| query_text::normalize(b).chars().collect()).collect();
    let counted: Vec<(String, usize)> = on_page
        .into_iter()
        .map(|w| {
            let n: Vec<char> = query_text::normalize(&w).chars().collect();
            let c = earlier.iter().map(|e| count_word(e, &n)).sum();
            (w, c)
        })
        .collect();
    // Fewest before, then the longer; a stable order keeps the query's order among equals.
    let mut best = &counted[0];
    for c in &counted[1..] {
        if c.1 < best.1 || (c.1 == best.1 && c.0.chars().count() > best.0.chars().count()) {
            best = c;
        }
    }
    Some(best.0.clone())
}

/// The whole word of `text` that `term` matches (its stem starts it), as spelled there.
///
/// Each word of the page is normalised on its own and compared: normalising the page at once and taking the match's
/// offsets back to the page's own text took the wrong characters wherever normalising changed the length (combining
/// accents, letters Unicode keeps decomposed), quotes included. Only letters, digits and marks are ever returned.
fn word_for(term: &KeywordTerm, text: &str) -> Option<String> {
    let is_word_char = |c: char| is_letter_or_digit(c) || is_mark(c) || c == '_';
    for token in text.split(|c: char| !is_word_char(c)).filter(|t| !t.is_empty()) {
        // "_" splits words for keyword search too: "parse" is a word of "parse_duration".
        let parts = std::iter::once(token).chain(token.split('_').filter(|p| !p.is_empty() && p.len() < token.len()));
        for word in parts {
            let n = query_text::normalize(word);
            if n == term.stem || (term.prefix && n.starts_with(&term.stem)) {
                return Some(word.to_string());
            }
        }
    }
    None
}

/// How often `word` stands in `text` as a whole word.
fn count_word(text: &[char], word: &[char]) -> usize {
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = crate::uni::find_chars(text, word, from) {
        let end = i + word.len();
        if (i == 0 || !is_letter_or_digit(text[i - 1])) && (end >= text.len() || !is_letter_or_digit(text[end])) {
            n += 1;
        }
        from = end.max(i + 1);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_least_often_on_earlier_pages() {
        let terms = query_text::terms("contract penalty termination", false);
        let before: Vec<String> =
            ["The contract between the parties. Termination is possible.", "More of the contract.", "Termination fees."]
                .map(String::from)
                .to_vec();
        assert_eq!(find_in("Penalty: the contract sets a penalty of 5%.", &before, &terms).map(|w| w.to_lowercase()).as_deref(), Some("penalty"));
        assert_eq!(find_in("This contract ends with termination.", &before, &terms).as_deref(), Some("termination"));
        assert_eq!(find_in("This contract ends.", &before, &terms).as_deref(), Some("contract"));
        assert_eq!(find_in("Nothing related here.", &before, &terms), None);
    }

    #[test]
    fn spelled_as_the_page_spells_it() {
        let terms = query_text::terms("кошка", false);
        assert_eq!(find_in("Две кошки спят на диване.", &[], &terms).as_deref(), Some("кошки"));
        assert_eq!(find_in("Café Penalty clause", &[], &query_text::terms("penalty", false)).as_deref(), Some("Penalty"));
        assert_eq!(find_in("see parse_duration here", &[], &query_text::terms("duration", false)).as_deref(), Some("duration"));
    }

    #[test]
    fn never_takes_characters_around_the_word() {
        // A decomposed accent shortens the normalised text and a Devanagari letter Unicode keeps decomposed lengthens it:
        // offsets taken across them once returned "\"penalt" (a quote went into the PDF viewer's command line).
        let page = "e\u{301} \"penalty \u{958}";
        assert_eq!(find_in(page, &[], &query_text::terms("penalty", false)).as_deref(), Some("penalty"));
    }
}
