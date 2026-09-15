// SPDX-License-Identifier: MIT OR Apache-2.0

//! Text normalization for prompts and tool descriptions.
//!
//! Two different normalizations are needed. `normalize_text` removes
//! differences that no human would call a change, and is what gets hashed.
//! `shape_text` additionally collapses interior whitespace, and exists only so
//! a formatting-only edit can be told apart from a semantic one without asking
//! a model.

/// Strip a BOM and normalize line endings.
fn unify(raw: &str) -> String {
    raw.strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Normalize text for hashing: line endings unified, trailing whitespace per
/// line removed, leading and trailing blank lines removed.
pub fn normalize_text(raw: &str) -> String {
    let unified = unify(raw);
    let mut lines: Vec<&str> = unified.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Collapse every whitespace run to a single space. Used to detect that a
/// change touched only formatting.
pub fn shape_text(raw: &str) -> String {
    unify(raw).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lf_produce_the_same_normalized_text() {
        assert_eq!(normalize_text("a\r\nb\r\n"), normalize_text("a\nb\n"));
    }

    #[test]
    fn trailing_whitespace_on_a_line_is_insignificant() {
        assert_eq!(normalize_text("a   \nb\t\n"), normalize_text("a\nb\n"));
    }

    #[test]
    fn a_leading_byte_order_mark_is_stripped() {
        assert_eq!(normalize_text("\u{feff}a"), "a");
    }

    #[test]
    fn surrounding_blank_lines_are_insignificant() {
        assert_eq!(normalize_text("\n\n  \na\nb\n\n"), "a\nb");
    }

    #[test]
    fn a_formatting_only_change_keeps_the_same_shape_but_changes_the_content() {
        let v1 = "Summarize   the repository.\n\nBe concise.";
        let v2 = "Summarize the repository.\n\nBe concise.\n";
        assert_eq!(shape_text(v1), shape_text(v2));
        assert_ne!(normalize_text(v1), normalize_text(v2));
    }

    #[test]
    fn a_semantic_change_changes_the_shape() {
        assert_ne!(shape_text("Be concise."), shape_text("Be thorough."));
    }
}
