//! Paragraph-level text splitting shared across ingest, compress, and digest.

/// Split `text` into paragraph-sized chunks.
/// A paragraph is a run of non-empty lines; blank lines are separators.
/// Chunks shorter than `min_chars` are merged with the next chunk.
/// Returns an owned Vec of paragraph strings (non-empty, trimmed).
pub fn split_paragraphs(text: &str, min_chars: usize) -> Vec<String> {
    let raw: Vec<String> = text
        .split('\n')
        .collect::<Vec<_>>()
        .split(|l: &&str| l.trim().is_empty())
        .map(|group| group.join("\n").trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();

    if min_chars == 0 {
        return raw;
    }

    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut pending: Option<String> = None;

    for chunk in raw {
        match pending.take() {
            Some(prev) if prev.len() < min_chars => {
                // merge short prev into current
                let merged = format!("{}\n\n{}", prev, chunk);
                pending = Some(merged);
            }
            Some(prev) => {
                out.push(prev);
                pending = Some(chunk);
            }
            None => {
                pending = Some(chunk);
            }
        }
    }
    if let Some(last) = pending {
        out.push(last);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert!(split_paragraphs("", 100).is_empty());
    }

    #[test]
    fn single_paragraph() {
        let out = split_paragraphs("hello world", 0);
        assert_eq!(out, vec!["hello world"]);
    }

    #[test]
    fn two_paragraphs_blank_line() {
        let text = "para one\n\npara two";
        let out = split_paragraphs(text, 0);
        assert_eq!(out, vec!["para one", "para two"]);
    }

    #[test]
    fn short_chunk_merged_into_next() {
        let text = "short\n\na longer paragraph that exceeds min_chars threshold";
        let out = split_paragraphs(text, 100);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("short"));
        assert!(out[0].contains("longer paragraph"));
    }

    #[test]
    fn min_chars_zero_no_merge() {
        let text = "a\n\nb\n\nc";
        let out = split_paragraphs(text, 0);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn trims_leading_trailing_whitespace() {
        let text = "  para one  \n\n  para two  ";
        let out = split_paragraphs(text, 0);
        assert_eq!(out, vec!["para one", "para two"]);
    }

    #[test]
    fn consecutive_blank_lines_treated_as_separator() {
        let text = "para one\n\n\n\npara two";
        let out = split_paragraphs(text, 0);
        assert_eq!(out, vec!["para one", "para two"]);
    }
}
