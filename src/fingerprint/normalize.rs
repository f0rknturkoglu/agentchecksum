// SPDX-License-Identifier: MIT OR Apache-2.0

//! Text normalization for prompts, tool descriptions, and chat templates.
//!
//! Two different normalizations, with deliberately different jobs.
//!
//! `normalize_text` feeds the **content** digest, which stays faithful to the text
//! that actually reaches the model. It removes only what a platform introduces on
//! its own: a leading BOM and CRLF line endings. Trailing spaces, leading and
//! trailing blank lines, a trailing newline, and interior whitespace all change
//! what the model sees, so all of them stay significant.
//!
//! `shape_text` feeds the **shape** digest. It collapses every whitespace run to a
//! single space so a formatting-only edit can be told apart from a semantic one
//! without asking a model. That is what lets Phase 2 classify a reflow as a
//! formatting change instead of having this layer decide on the user's behalf that
//! it was insignificant.

/// Strip a leading BOM and unify line endings.
fn unify(raw: &str) -> String {
    raw.strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// The text as the model receives it, modulo a leading BOM and platform line
/// endings — and nothing else.
pub fn normalize_text(raw: &str) -> String {
    unify(raw)
}

/// Collapse every whitespace run to a single space, for formatting-only detection.
pub fn shape_text(raw: &str) -> String {
    unify(raw).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lf_produce_the_same_content() {
        assert_eq!(normalize_text("a\r\nb\r\n"), normalize_text("a\nb\n"));
    }

    #[test]
    fn a_lone_carriage_return_is_a_line_ending() {
        assert_eq!(normalize_text("a\rb"), normalize_text("a\nb"));
    }

    #[test]
    fn a_leading_byte_order_mark_is_stripped() {
        assert_eq!(normalize_text("\u{feff}a"), "a");
    }

    #[test]
    fn a_leading_blank_line_changes_the_content_but_not_the_shape() {
        assert_ne!(normalize_text("a\nb"), normalize_text("\na\nb"));
        assert_eq!(shape_text("a\nb"), shape_text("\na\nb"));
    }

    #[test]
    fn a_trailing_blank_line_changes_the_content_but_not_the_shape() {
        assert_ne!(normalize_text("a\nb"), normalize_text("a\nb\n\n"));
        assert_eq!(shape_text("a\nb"), shape_text("a\nb\n\n"));
    }

    #[test]
    fn a_trailing_newline_is_significant() {
        // The most common accidental edit there is. It changes what the model
        // receives, so it is a content change; the shape digest stays equal, which
        // is what lets Phase 2 call it formatting-only rather than ignoring it here.
        assert_ne!(normalize_text("a\nb"), normalize_text("a\nb\n"));
        assert_eq!(shape_text("a\nb"), shape_text("a\nb\n"));
    }

    #[test]
    fn trailing_spaces_change_the_content_but_not_the_shape() {
        assert_ne!(normalize_text("a\nb"), normalize_text("a   \nb\t"));
        assert_eq!(shape_text("a\nb"), shape_text("a   \nb\t"));
    }

    #[test]
    fn interior_whitespace_changes_the_content_but_not_the_shape() {
        assert_ne!(normalize_text("a  b"), normalize_text("a b"));
        assert_eq!(shape_text("a  b"), shape_text("a b"));
    }

    #[test]
    fn a_semantic_edit_changes_both() {
        assert_ne!(
            normalize_text("Be concise."),
            normalize_text("Be thorough.")
        );
        assert_ne!(shape_text("Be concise."), shape_text("Be thorough."));
    }

    #[test]
    fn shape_collapses_every_whitespace_run() {
        assert_eq!(shape_text("a\n\nb\tc"), "a b c");
    }
}
