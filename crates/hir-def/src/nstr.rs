//! `НСтр` / `NStr` strings: one quoted text per language, `ru = 'Текст'; en = 'Text'`.

use stdx::case::CaseExt;

/// Whether `name` names the global function `НСтр` / `NStr`.
pub fn is_nstr_name(name: &str) -> bool {
    matches!(name.fold_lower().as_str(), "нстр" | "nstr")
}

/// Language entries of an `НСтр` literal as `(lowercased key, value)`. The value is `None`
/// when the quoted text is never closed or its closing quote is not followed by `;` or the
/// end of the text (`ru='' на '` - the platform's reading of that is unclear). The scan
/// steps over a whole quoted value, so text inside a value is never read as a key.
pub fn extract_language_entries(text: &str) -> Vec<(String, Option<String>)> {
    let mut entries = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !(chars[i].is_alphabetic() || chars[i] == '_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();

        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || chars[i] != '=' {
            continue;
        }
        i += 1;
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || (chars[i] != '\'' && chars[i] != '"') {
            continue;
        }

        let quote = chars[i];
        i += 1;
        let mut value = String::new();
        let mut closed = false;
        while i < len {
            if chars[i] == quote {
                if i + 1 < len && chars[i + 1] == quote {
                    value.push(quote);
                    i += 2;
                    continue;
                }
                closed = true;
                i += 1;
                break;
            }
            value.push(chars[i]);
            i += 1;
        }
        let mut rest = i;
        while rest < len && chars[rest].is_whitespace() {
            rest += 1;
        }
        let clean_end = rest >= len || chars[rest] == ';';
        entries.push((ident.fold_lower(), (closed && clean_end).then_some(value)));
    }

    entries
}
