//! `СтрШаблон` / `StrTemplate` templates: the texts a template expression can take and the
//! check of each text against the number of arguments of the call.
//!
//! Lowering checks a template whose texts follow from the expression alone
//! ([`constant_template_texts`]); `ide-diagnostics` checks one that reads a variable and
//! passes [`template_texts`] the variable's texts from reaching definitions.

use stdx::case::CaseExt;

use crate::body::Body;
use crate::hir::{BinaryOp, Expr, ExprIdx, Literal};
use crate::nstr::{extract_language_entries, is_nstr_name};
use crate::Name;

/// More texts than this leave a template unknown: each `?(...)` and each language of an
/// `НСтр` multiplies the texts of whatever is added to it.
const MAX_TEMPLATE_TEXTS: usize = 16;

/// Whether `name` names the global function `СтрШаблон` / `StrTemplate`.
pub fn is_str_template_name(name: &str) -> bool {
    matches!(name.fold_lower().as_str(), "стршаблон" | "strtemplate")
}

/// [`template_texts`] of a template that reads no variable.
pub fn constant_template_texts(body: &Body, expr: ExprIdx) -> Option<Vec<String>> {
    template_texts(body, expr, &mut |_: &Name| None)
}

/// Every text the template expression `expr` can take, or `None` when a part of it is
/// unknown: such a part may carry markers of its own.
///
/// A known part is a string literal, a `+` of known parts, a `?(...)` whose branches are
/// both known (each branch gives its texts), a character of `Символы` / `Chars` (text with
/// no marker), or `НСтр` / `NStr` of a known text, which gives each translation it can
/// return. `resolve_name` gives the texts of a variable, `None` when they are unknown.
pub fn template_texts(
    body: &Body,
    expr: ExprIdx,
    resolve_name: &mut dyn FnMut(&Name) -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    let texts = match body.expr_idx(expr) {
        Expr::Literal(Literal::String(text)) => vec![text.clone()],
        Expr::BinaryOp { lhs, rhs, op: BinaryOp::Add } => {
            let lhs = template_texts(body, *lhs, resolve_name)?;
            let rhs = template_texts(body, *rhs, resolve_name)?;
            let mut texts = Vec::with_capacity(lhs.len() * rhs.len());
            for left in &lhs {
                for right in &rhs {
                    texts.push(format!("{left}{right}"));
                }
            }
            texts
        }
        Expr::Ternary { then_expr, else_expr, .. } => {
            let mut texts = template_texts(body, *then_expr, resolve_name)?;
            texts.extend(template_texts(body, *else_expr, resolve_name)?);
            texts
        }
        Expr::Call { callee, args } if is_path_named(body, *callee, is_nstr_name) => {
            nstr_texts(body, args, resolve_name)?
        }
        Expr::Field { base, field } if is_path_named(body, *base, is_chars_name) => {
            vec![chars_property(field.as_str())?.to_string()]
        }
        Expr::Path(name) => resolve_name(name)?,
        _ => return None,
    };
    distinct_within_limit(texts)
}

/// The texts `НСтр(source, language)` can return: every translation of each source text,
/// or only the one of `language` when the language is known.
fn nstr_texts(
    body: &Body,
    args: &[ExprIdx],
    resolve_name: &mut dyn FnMut(&Name) -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    let sources = template_texts(body, *args.first()?, resolve_name)?;
    let languages = match args.get(1) {
        Some(&language) => template_texts(body, language, resolve_name),
        None => None,
    };
    let mut texts = Vec::new();
    for source in &sources {
        texts.extend(translations(source, languages.as_deref())?);
    }
    Some(texts)
}

/// The translations of `source` that `НСтр` can return, or `None` when a known language has
/// no translation in it. A source without language entries, or with a value that is not
/// closed, stays one text as written: the platform's reading of it is unclear.
fn translations(source: &str, languages: Option<&[String]>) -> Option<Vec<String>> {
    let entries: Option<Vec<(String, String)>> = extract_language_entries(source)
        .into_iter()
        .map(|(language, value)| value.map(|value| (language, value)))
        .collect();
    let entries = match entries {
        Some(entries) if !entries.is_empty() => entries,
        _ => return Some(vec![source.to_owned()]),
    };
    let Some(languages) = languages else {
        return Some(entries.into_iter().map(|(_, value)| value).collect());
    };
    languages
        .iter()
        .map(|language| {
            let language = language.trim().fold_lower();
            entries.iter().find(|(key, _)| *key == language).map(|(_, value)| value.clone())
        })
        .collect()
}

fn distinct_within_limit(texts: Vec<String>) -> Option<Vec<String>> {
    let mut distinct: Vec<String> = Vec::with_capacity(texts.len());
    for text in texts {
        if !distinct.contains(&text) {
            distinct.push(text);
        }
    }
    (distinct.len() <= MAX_TEMPLATE_TEXTS).then_some(distinct)
}

fn is_path_named(body: &Body, expr: ExprIdx, is_name: fn(&str) -> bool) -> bool {
    matches!(body.expr_idx(expr), Expr::Path(name) if is_name(name.as_str()))
}

fn is_chars_name(name: &str) -> bool {
    matches!(name.fold_lower().as_str(), "символы" | "chars")
}

/// The character a property of `Символы` / `Chars` holds.
fn chars_property(name: &str) -> Option<char> {
    let character = match name.fold_lower().as_str() {
        "пс" | "lf" => '\n',
        "вк" | "cr" => '\r',
        "таб" | "tab" => '\t',
        "втаб" | "vtab" => '\u{b}',
        "пф" | "ff" => '\u{c}',
        "нпп" | "nbsp" => '\u{a0}',
        _ => return None,
    };
    Some(character)
}

/// Whether `template` does not fit `used_params_count` arguments: a marker with no
/// argument, an argument with no marker, or a marker number outside `%1`..`%10`.
pub fn is_wrong_str_template(template: &str, used_params_count: usize) -> bool {
    let is_wrong_call = compare_template_and_params(template, used_params_count);
    if !is_wrong_call {
        return false;
    }

    let cleaned = remove_double_percent(template);
    compare_template_and_params(&cleaned, used_params_count)
}

/// The highest marker number of `template`, `%10` at most.
pub fn count_required_params(template: &str) -> usize {
    let bytes = template.as_bytes();
    let mut max_param = 0;

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                i += 2;
                continue;
            }

            if let Some((num, len)) = parse_placeholder(bytes, i) {
                if is_valid_placeholder(num) {
                    max_param = max_param.max(num);
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }

    max_param
}

fn remove_double_percent(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'%' && bytes[i + 1] == b'%' {
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn parse_placeholder(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    if pos >= bytes.len() || bytes[pos] != b'%' {
        return None;
    }

    let start = pos + 1;
    if start >= bytes.len() {
        return None;
    }

    if bytes[start] == b'(' {
        let num_start = start + 1;
        let mut num_end = num_start;
        while num_end < bytes.len() && bytes[num_end].is_ascii_digit() {
            num_end += 1;
        }
        if num_end > num_start && num_end < bytes.len() && bytes[num_end] == b')' {
            let num_str = std::str::from_utf8(&bytes[num_start..num_end]).ok()?;
            let num: usize = num_str.parse().ok()?;
            return Some((num, num_end - pos + 1));
        }
        return None;
    }

    let mut num_end = start;
    while num_end < bytes.len() && bytes[num_end].is_ascii_digit() {
        num_end += 1;
    }
    if num_end > start {
        let num_str = std::str::from_utf8(&bytes[start..num_end]).ok()?;
        let num: usize = num_str.parse().ok()?;
        return Some((num, num_end - pos));
    }

    None
}

fn is_valid_placeholder(num: usize) -> bool {
    (1..=10).contains(&num)
}

// Each early return below is one kind of mismatch; the merged condition clippy proposes
// would hide which one fires.
#[allow(clippy::nonminimal_bool)]
fn compare_template_and_params(template: &str, used_params_count: usize) -> bool {
    let bytes = template.as_bytes();
    let have_params = used_params_count > 0;

    let mut has_valid_placeholder = false;
    let mut has_wrong_number = false;
    let mut used_placeholders = [false; 11];

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                i += 2;
                continue;
            }

            if let Some((num, len)) = parse_placeholder(bytes, i) {
                if is_valid_placeholder(num) {
                    has_valid_placeholder = true;
                    used_placeholders[num] = true;
                    if num > used_params_count {
                        return true;
                    }
                } else {
                    has_wrong_number = true;
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }

    if has_wrong_number {
        return true;
    }
    if has_valid_placeholder && !have_params {
        return true;
    }
    if !has_valid_placeholder && have_params {
        return true;
    }

    if has_valid_placeholder {
        for &used in used_placeholders.iter().take(used_params_count + 1).skip(1) {
            if !used {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|item| item.to_string()).collect())
    }

    #[test]
    fn translations_split_a_multilingual_text_by_language() {
        let source = "ru = 'Документ %1'; en = 'Document'";
        assert_eq!(translations(source, None), texts(&["Документ %1", "Document"]));
        assert_eq!(translations(source, Some(&["EN".to_string()][..])), texts(&["Document"]));
        assert_eq!(translations(source, Some(&["uk".to_string()][..])), None);
    }

    #[test]
    fn translations_keep_a_text_without_closed_entries_as_written() {
        assert_eq!(translations("Документ %1", None), texts(&["Документ %1"]));
        assert_eq!(translations("ru = 'Документ %1", None), texts(&["ru = 'Документ %1"]));
    }

    #[test]
    fn a_marker_without_argument_and_an_argument_without_marker_are_wrong() {
        assert!(!is_wrong_str_template("Документ %1 от %2", 2));
        assert!(is_wrong_str_template("Документ %1 от %2", 1));
        assert!(is_wrong_str_template("Документ %1", 2));
        assert!(is_wrong_str_template("Документ\n", 1));
        assert_eq!(count_required_params("Документ %1 от %(2)"), 2);
    }
}
