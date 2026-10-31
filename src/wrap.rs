//! Soft-wrapping by display width. Spaces are kept (so indented code stays indented); a line breaks at
//! its last space when one exists inside the width, and in the middle of a word otherwise.

use unicode_width::UnicodeWidthChar;

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        wrap_paragraph(paragraph.trim_end_matches('\r'), width, &mut out);
    }
    out
}

fn wrap_paragraph(text: &str, width: usize, out: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_width = 0;
    // Byte offset in `line` just after the last space.
    let mut last_break: Option<usize> = None;

    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if line_width + w > width && !line.is_empty() {
            match last_break {
                // Break at a space only when some text comes before it, so indentation is not stranded.
                Some(at) if !line[..at].trim().is_empty() => {
                    let rest = line.split_off(at);
                    out.push(line.trim_end().to_string());
                    line = rest;
                }
                _ => out.push(std::mem::take(&mut line)),
            }
            line_width = line.chars().map(|c| c.width().unwrap_or(0)).sum();
            last_break = None;
        }
        line.push(ch);
        line_width += w;
        if ch == ' ' {
            last_break = Some(line.len());
        }
    }
    out.push(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width_of(s: &str) -> usize {
        s.chars().map(|c| c.width().unwrap_or(0)).sum()
    }

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(wrap("hello world", 20), ["hello world"]);
    }

    #[test]
    fn breaks_at_spaces() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
    }

    #[test]
    fn breaks_long_words_in_the_middle() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn keeps_newlines_and_blank_lines() {
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn keeps_indentation() {
        assert_eq!(wrap("    let x = 1;", 40), ["    let x = 1;"]);
    }

    #[test]
    fn wide_characters_count_double() {
        let lines = wrap("日本語日本語", 6);
        assert_eq!(lines, ["日本語", "日本語"]);
        assert!(lines.iter().all(|l| width_of(l) <= 6));
    }

    #[test]
    fn no_line_exceeds_the_width_and_no_text_is_lost() {
        let text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore";
        for width in [1, 2, 5, 17, 40] {
            let lines = wrap(text, width);
            assert!(
                lines.iter().all(|l| width_of(l) <= width.max(1)),
                "width {width}: {lines:?}"
            );
            let joined: String = lines.concat();
            let original: String = text.split(' ').collect();
            assert_eq!(joined.replace(' ', ""), original, "width {width}");
        }
    }

    #[test]
    fn long_word_after_indentation_does_not_leave_an_empty_line() {
        assert_eq!(wrap("    abcdefghij", 6), ["    ab", "cdefgh", "ij"]);
    }

    #[test]
    fn crlf_is_treated_as_a_newline() {
        assert_eq!(wrap("a\r\nb", 10), ["a", "b"]);
    }
}
