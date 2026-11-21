// FTS5's trigram tokenizer needs three Unicode scalars. Framing each scalar with
// a shared boundary makes even one character a searchable trigram. A quoted word
// still matches a contiguous substring, with SQLite's original Unicode folding.
const BOUNDARY: char = '\u{1f}';
const ESCAPE: char = '\u{1e}';

pub(super) fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() * 2 + 1);
    append_literal(&mut encoded, text);
    encoded
}

// Two adjacent boundaries separate independent strings. No encoded query can
// contain that sequence, even when the source contains a boundary character.
pub(super) fn append_literal(encoded: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    encoded.push(BOUNDARY);
    for ch in text.chars() {
        match ch {
            BOUNDARY | ESCAPE | '\0' => {
                encoded.push(ESCAPE);
                encoded.push(match ch {
                    BOUNDARY => 'b',
                    ESCAPE => 'e',
                    _ => '0',
                });
            }
            _ => encoded.push(ch),
        }
        encoded.push(BOUNDARY);
    }
}

pub(super) fn expression(query: &str, original: bool, phrase: bool) -> String {
    if original {
        // Keep the existing literal trigram index and its original-text snippets.
        return format!("{{title body}} : \"{}\"", query.replace('"', "\"\""));
    }
    let quote = |word: &str| format!("\"{}\"", encode(word).replace('"', "\"\""));
    if phrase {
        return quote(query);
    }
    let words = query
        .split_whitespace()
        .map(quote)
        .collect::<Vec<_>>()
        .join(" AND ");
    format!("{{title_terms body_terms}} : ({words})")
}

pub(super) fn decode(snippet: &str) -> String {
    let mut text = String::new();
    let mut chars = snippet.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            BOUNDARY => {
                if chars.next_if_eq(&BOUNDARY).is_some() {
                    text.push('…');
                }
            }
            ESCAPE => match chars.next() {
                Some('b') => text.push(BOUNDARY),
                Some('e') => text.push(ESCAPE),
                Some('0') => text.push('\0'),
                Some(ch) => text.push(ch),
                None => {}
            },
            _ => text.push(ch),
        }
    }
    text
}
