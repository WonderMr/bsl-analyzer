//! Response-shaping helpers shared by the agent-facing tools.

use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::Value;

/// Emit `body` as the MCP `structuredContent` field. rmcp mirrors the value as a
/// compact JSON text block for clients without structured-output support;
/// structured-aware hosts read `structuredContent` and ignore the mirror, so the
/// payload reaches a model exactly once either way.
pub fn structured(body: Value) -> CallToolResult {
    CallToolResult::structured(body)
}

/// Default output budget (in tokens, ~4 chars each) for the list/text tools that carry no
/// explicit per-action default, mirroring `graph`'s body budget. A small `limit` keeps most
/// responses well under this, so the default rarely bites; it is a ceiling, not a target.
pub const DEFAULT_OUTPUT_BUDGET_TOKENS: usize = 6000;

pub(crate) fn serialized_bytes<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// The output-budget convention shared by the list-returning tools, mirroring `graph`'s
/// body budget. Callers sort `items` into a stable order first, then drop trailing items
/// until the serialized array fits `max_output_tokens` (~4 chars each), keeping at least
/// one item so a single oversized item is still delivered. Returns `true` when anything
/// was dropped, so the caller can stamp `budget_exhausted` and a continuation hint.
///
/// Item sizes are measured once (O(n)); the `+1` per gap is the JSON comma, `+2` the
/// enclosing brackets — a close-enough upper bound on the array's own framing.
pub fn trim_items_to_budget(items: &mut Vec<Value>, max_output_tokens: usize) -> bool {
    let budget = max_output_tokens.saturating_mul(4);
    let mut used = 2usize; // `[` + `]`
    let mut keep = 0usize;
    for (i, item) in items.iter().enumerate() {
        let len = serde_json::to_string(item).map(|s| s.len()).unwrap_or(0);
        let sep = usize::from(i > 0); // comma between items
        let next = used + sep + len;
        if next > budget && keep > 0 {
            break;
        }
        used = next;
        keep = i + 1;
    }
    let dropped = keep < items.len();
    if dropped {
        items.truncate(keep.max(1));
    }
    // Exhausted means "the returned array may exceed the budget", so it fires either when we
    // dropped a trailing item OR when the single kept item is itself over budget (the keep>=1
    // floor delivers it anyway) — the flag never depends on how many siblings it had.
    dropped || used > budget
}

/// Truncate a Markdown/plain-text tool body to fit `max_output_tokens` (~4 chars each) at a
/// line boundary, appending `note` (a continuation hint) when it had to cut. Text tools carry
/// no structured envelope, so the trailing note IS the truncation marker. Returns whether it
/// truncated.
///
/// A line longer than half the budget — a long signature, one serialized value — is cut at a
/// char boundary rather than dropped whole: falling back to the line break before it would keep
/// next to nothing. A cut inside a fenced code block closes the fence before the note, or the
/// note would render as code.
///
/// The note is the floor: a budget smaller than `note` yields the note alone rather than a
/// silently clipped body, because an unmarked truncation reads as a complete answer.
pub fn truncate_text_to_budget(text: &mut String, max_output_tokens: usize, note: &str) -> bool {
    let budget = max_output_tokens.saturating_mul(4);
    if text.len() <= budget {
        return false;
    }
    // Reserve room for the note so the final string (body + note) still fits the budget rather
    // than overshooting it by the note's length, and for the closing fence when the cut falls
    // inside a code block.
    let mut cut = cut_point(text, budget.saturating_sub(note.len()));
    if inside_code_fence(&text[..cut]) {
        cut = cut_point(text, budget.saturating_sub(note.len() + FENCE_CLOSE.len()));
    }
    text.truncate(cut);
    if inside_code_fence(text) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }
    text.push_str(note);
    true
}

/// The longest fence close [`truncate_text_to_budget`] appends: a line break and the fence.
const FENCE_CLOSE: &str = "\n```";

/// Where to cut `text` so that at most `cap` bytes are kept: after the last line break within
/// them, unless that keeps less than half of `cap`, then at the char boundary nearest `cap`.
fn cut_point(text: &str, cap: usize) -> usize {
    let mut cap = cap.min(text.len());
    while cap > 0 && !text.is_char_boundary(cap) {
        cap -= 1;
    }
    match text[..cap].rfind('\n') {
        Some(line_end) if (line_end + 1) * 2 >= cap => line_end + 1,
        _ => cap,
    }
}

/// Whether `text` ends inside a fenced code block: an odd number of its lines open a fence.
fn inside_code_fence(text: &str) -> bool {
    !text.lines().filter(|line| line.trim_start().starts_with("```")).count().is_multiple_of(2)
}

/// Emit a plain-text tool body already fitted to `max_output_tokens`, appending `note` when it
/// had to cut. The text tools carry no structured envelope, so this is the whole truncation
/// contract for them: body first, continuation hint last.
pub fn text_within_budget(
    mut text: String,
    max_output_tokens: usize,
    note: &str,
) -> CallToolResult {
    truncate_text_to_budget(&mut text, max_output_tokens, note);
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Emit `body` as `structuredContent` while keeping `text` — a human-readable rendering, not
/// the serialized body — as the content block. For a tool whose text listing predates its
/// structured envelope: existing clients keep the exact text they parse today, structured
/// clients read the envelope. Both describe the same answer, so callers must budget the pair
/// together (see [`crate::tools::search`]), not the text alone.
pub fn structured_with_text(text: String, body: Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(body);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn items(n: usize) -> Vec<serde_json::Value> {
        (0..n).map(|i| json!({ "code": format!("Rule{i:03}"), "text": "x".repeat(40) })).collect()
    }

    #[test]
    fn trim_keeps_all_when_within_budget() {
        let mut v = items(3);
        // 3 items × ~60 chars is far under 1000 tokens (~4000 chars).
        assert!(!trim_items_to_budget(&mut v, 1000));
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn trim_drops_trailing_items_over_budget() {
        let mut v = items(50);
        // 1 token ≈ 4 chars: only the first item (well over 4 chars) survives, never zero.
        let dropped = trim_items_to_budget(&mut v, 1);
        assert!(dropped);
        assert_eq!(v.len(), 1);
        // The stable-order head is what remains.
        assert_eq!(v[0]["code"], "Rule000");
    }

    #[test]
    fn trim_flags_single_oversized_item_even_though_none_dropped() {
        // One item bigger than the budget: the keep>=1 floor delivers it, but the caller must
        // still learn the output exceeds the budget — so exhausted is true with nothing dropped.
        let mut v = items(1);
        assert!(trim_items_to_budget(&mut v, 1));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn structured_with_text_keeps_the_human_rendering_as_the_content_block() {
        let result = structured_with_text("#1 hit".to_owned(), json!({ "hits": [] }));

        assert_eq!(result.content[0].as_text().expect("text").text, "#1 hit");
        assert_eq!(result.structured_content.expect("structured"), json!({ "hits": [] }));
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn truncate_text_cuts_at_line_boundary_and_notes() {
        let mut text = "line-a\nline-b\nline-c\nline-d\n".to_string();
        // Budget 5 tokens = 20 chars, note 12 chars: after reserving the note there is room for
        // the first whole line only.
        let cut = truncate_text_to_budget(&mut text, 5, "\n-- more --\n");
        assert!(cut);
        assert!(text.starts_with("line-a\n"));
        assert!(!text.contains("line-d"));
        assert!(text.ends_with("-- more --\n"));
        // Never leaves a half-written line before the note.
        assert!(!text.contains("line-b\nline-c"));
    }

    /// One line longer than half the budget is cut inside: the line break before it would keep
    /// the heading alone.
    #[test]
    fn truncate_text_cuts_a_long_line_at_a_char_boundary() {
        let mut text = format!("# Заголовок\n\n{}\n", "сигнатура ".repeat(40));
        assert!(truncate_text_to_budget(&mut text, 50, "\n--\n"));
        assert!(text.len() <= 200, "{} bytes", text.len());
        assert!(text.starts_with("# Заголовок\n\nсигнатура сигнатура"), "{text}");
        assert!(text.ends_with("\n--\n"), "{text}");
    }

    /// A cut inside a fenced block closes the fence, or the note would read as code.
    #[test]
    fn truncate_text_closes_a_code_fence_it_cuts_into() {
        let lines: String = (0..40).map(|n| format!("line-{n:02}\n")).collect();
        let mut text = format!("intro\n```bsl\n{lines}```\nafter\n");
        assert!(truncate_text_to_budget(&mut text, 25, "\n--\n"));
        assert!(text.len() <= 100, "{} bytes", text.len());
        assert!(text.ends_with("line-08\n```\n--\n"), "{text}");
    }

    #[test]
    fn truncate_text_noop_when_within_budget() {
        let mut text = "short\n".to_string();
        assert!(!truncate_text_to_budget(&mut text, 1000, "-- more --"));
        assert_eq!(text, "short\n");
    }
}
