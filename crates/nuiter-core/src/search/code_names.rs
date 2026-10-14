//! Names in code. A query that is a name (as code spells it, not as people talk) is looked up as that name, and the
//! result opens where the name is defined rather than where it is used. A code result opens at the line that matched.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::uni::{is_digit, is_letter_or_digit, is_lower, is_upper, lower};

static NAME_SHAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}_$][\p{L}\p{N}_$]*(?:(?:\.|::|->)[\p{L}_$][\p{L}\p{N}_$]*)*$").unwrap());
static STARTS_DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected|internal|static|async|export|override|virtual|abstract|pub)\s+)*(?:def|class|struct|enum|interface|trait|func|fn|fun|function|sub|module|type|record|impl|#define)\b").unwrap()
});

/// The name a query asks for, or None when it reads like ordinary words. A name is one token (dots, :: and -> may join
/// parts; a trailing "()" is dropped) that people wouldn't type in prose: it has an underscore, a digit, a separator,
/// or a lowercase letter followed by a capital ("parseHeader", "HttpClient").
pub fn name_in(query: &str) -> Option<String> {
    let mut name = query.trim();
    if let Some(n) = name.strip_suffix("()") {
        name = n;
    }
    if !NAME_SHAPE.is_match(name) {
        return None;
    }
    let chars: Vec<char> = name.chars().collect();
    let humped = chars.windows(2).any(|w| is_lower(w[0]) && is_upper(w[1]));
    (humped || name.contains(['_', '.', ':', '>', '$']) || name.chars().any(is_digit)).then(|| name.to_string())
}

/// What a definition spells: the last part of a qualified name ("System.IO.Path" → "Path").
pub fn last_part(identifier: &str) -> &str {
    match identifier.rfind(['.', ':', '>']) {
        Some(cut) if cut < identifier.len() - 1 => &identifier[cut + 1..],
        _ => identifier,
    }
}

/// Where `name` is defined in `text` (lines numbered from `first`), and how sure that is: 2 for a definition, 1 for a
/// declaration without a body (a prototype ending in ";"). None when the text only uses the name.
pub fn find_definition(name: &str, text: &str, first: usize) -> Option<(usize, u8)> {
    let n = regex::escape(name);
    // In order: introduced by a keyword (def, class, fn, func, …; #define), a Go method, an Objective-C method, a
    // function value bound to it (JavaScript), a C-family function or method (a type before it, a parameter list after
    // it), a C# property.
    let patterns: [(String, bool); 6] = [
        (format!(r"(?i)(^|[\s;{{(])(?:def|defp|class|struct|enum|interface|trait|protocol|record|func|fn|fun|function\*?|sub|module|namespace|type|typedef|typealias|union|impl|object|actor|extension|macro_rules!|#define)\s+(?:[\w.]+\.)?{n}\b"), false),
        (format!(r"^\s*func\s*\([^)]*\)\s*{n}\s*[\[(]"), false),
        (format!(r"^\s*[-+]\s*\([^)]*\)\s*{n}\b"), false),
        (format!(r"\b(?:const|let|var)\s+{n}\s*=\s*(?:async\b\s*)?(?:function\b|\([^)]*\)\s*=>|[\w$]+\s*=>)"), false),
        (format!(r"^\s*(?:[\w$<>\[\],.?*&:]+\s+)+[*&]*{n}\s*(?:<[^<>()]*>)?\s*\("), true),
        (format!(r"^\s*(?:(?:public|private|protected|internal|static|virtual|override|abstract|sealed|required|readonly)\s+)+[\w<>\[\],.?]+\s+{n}\s*(?:\{{|=>)"), false),
    ];
    let compiled: Vec<(Regex, bool)> = patterns.iter().filter_map(|(p, typed)| Regex::new(p).ok().map(|r| (r, *typed))).collect();
    let mut best: Option<(usize, u8)> = None;
    let normalized = text.replace("\r\n", "\n");
    for (i, line) in normalized.split('\n').enumerate() {
        if !line.contains(name) || is_comment(line) {
            continue;
        }
        for (pattern, typed) in &compiled {
            if !pattern.is_match(line) {
                continue;
            }
            if *typed && !looks_declared(line, name) {
                continue;
            }
            let strength = if *typed && line.trim_end().ends_with(';') && !line.contains("=>") { 1 } else { 2 };
            if strength == 2 {
                return Some((first + i, 2));
            }
            if best.is_none() {
                best = Some((first + i, 1));
            }
        }
    }
    best
}

/// Words a call can follow inside a statement: what comes after them is used, not defined.
static EXPRESSION_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "return", "await", "yield", "throw", "new", "else", "case", "in", "is", "as", "not", "and", "or", "do", "go",
        "defer", "echo", "print", "puts", "assert", "raise", "typeof", "sizeof", "nameof", "delete", "using", "import",
        "from", "goto", "when", "then", "lock", "co_await", "co_return",
    ]
    .into_iter()
    .collect()
});

/// For "type name(…)": the words before the name are a type and modifiers, not a call in an expression ("return
/// name(", "x = name(", "if (name(").
fn looks_declared(line: &str, name: &str) -> bool {
    let Some(at) = line.find(name) else { return false };
    let before = line[..at].trim();
    if before.is_empty() || before.contains(['=', '(', ',', '!', '?', '+', '|', '"', '\'']) || before.contains("->") {
        return false;
    }
    let last_word = before.split(' ').rfind(|w| !w.is_empty()).unwrap_or("").trim_end_matches(['*', '&']);
    !EXPRESSION_WORDS.contains(last_word)
}

/// The line to open a code piece at (numbered from `first`) for `words`: the line with the most of them, where a line
/// that defines something counts more and a comment less; the earliest of equal lines. A word counts at a word's start
/// or at a part of a name (retry in retryWithBackoff, retry_count). None when no line has any of them.
pub fn best_line<'a>(words: impl IntoIterator<Item = &'a str>, text: &str, first: usize) -> Option<usize> {
    let mut list: Vec<String> = vec![];
    for w in words {
        let l: String = w.chars().map(lower).collect();
        if l.chars().count() >= 2 && !list.contains(&l) {
            list.push(l);
        }
    }
    if list.is_empty() {
        return None;
    }
    let normalized = text.replace("\r\n", "\n");
    let mut best = None;
    let mut best_score = 0.0;
    for (i, line) in normalized.split('\n').enumerate() {
        let found = list.iter().filter(|w| has_part(line, w)).count();
        if found == 0 {
            continue;
        }
        let score = found as f64 + if STARTS_DEFINITION.is_match(line) { 2.0 } else { 0.0 } - if is_comment(line) { 0.5 } else { 0.0 };
        if score > best_score {
            best = Some(first + i);
            best_score = score;
        }
    }
    best
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("--")
        || t.starts_with("<!--")
        || (t.starts_with('#') && !t.starts_with("#define") && !t.starts_with("#include"))
        || t.starts_with('\'')
        || t.starts_with("\"\"\"")
}

/// Whether `line` holds `word` (lowercase) where a word or a part of a name starts: at the line's start, after a
/// character that isn't a letter or digit, or at a capital.
fn has_part(line: &str, word: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let lowered: Vec<char> = chars.iter().map(|&c| lower(c)).collect();
    let w: Vec<char> = word.chars().collect();
    let mut from = 0;
    while let Some(at) = crate::uni::find_chars(&lowered, &w, from) {
        if at == 0 || !is_letter_or_digit(chars[at - 1]) || is_upper(chars[at]) {
            return true;
        }
        from = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_names_are_recognised_and_their_definitions_found() {
        assert_eq!(name_in("parseHeader()").as_deref(), Some("parseHeader"));
        assert_eq!(name_in("HttpClient").as_deref(), Some("HttpClient"));
        assert_eq!(name_in("read_block_v2").as_deref(), Some("read_block_v2"));
        assert_eq!(name_in("System.IO.Path").as_deref(), Some("System.IO.Path"));
        assert_eq!(name_in("retry"), None);
        assert_eq!(name_in("Renamer"), None);
        assert_eq!(name_in("retry a failed call"), None);
        assert_eq!(last_part("System.IO.Path"), "Path");
        assert_eq!(last_part("Store::open"), "open");

        assert_eq!(find_definition("parse_duration", "import x\n\ndef parse_duration(text):", 10), Some((12, 2)));
        assert_eq!(find_definition("RetryAsync", "public async Task<int> RetryAsync(Func<Task> f) {", 5), Some((5, 2)));
        assert_eq!(
            find_definition("parser_open", "p = parser_open(path);\nstatic struct parser_state * parser_open(const char *path) {", 6),
            Some((7, 2))
        );
        assert_eq!(find_definition("parser_open", "struct parser_state * parser_open(const char *path);", 1), Some((1, 1)));
        assert_eq!(
            find_definition("loadOrders", "// loadOrders below\nimport x from 'y';\nexport const loadOrders = async (id) => fetch(id);", 1),
            Some((3, 2))
        );
        assert_eq!(find_definition("Count", "// Count\npublic int Count => _items.Length;", 1), Some((2, 2)));
        assert_eq!(find_definition("parse_duration", "    seconds = parse_duration(spec)\n    return await parse_duration(g)", 1), None);
        assert_eq!(best_line(["backoff"], "# backoff notes\nx = 1\nwait = backoff(2)", 1), Some(3));
    }
}
