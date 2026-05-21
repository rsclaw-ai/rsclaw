//! Paragraph splitter on blank lines. Preserves byte offsets so
//! the chunker can record exact `(start, end)` of each chunk in the
//! source markdown.

#[derive(Debug, Clone, PartialEq)]
pub struct Paragraph {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

pub fn split_paragraphs(md: &str) -> Vec<Paragraph> {
    let bytes = md.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            push(&mut out, md, start, i);
            i += 2;
            while i < bytes.len() && bytes[i] == b'\n' {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    push(&mut out, md, start, bytes.len());
    out
}

fn push(out: &mut Vec<Paragraph>, md: &str, start: usize, end: usize) {
    let slice = &md[start..end];
    let t = slice.trim();
    if !t.is_empty() {
        let leading = slice.len() - slice.trim_start().len();
        let trailing = slice.len() - slice.trim_end().len();
        out.push(Paragraph {
            start: start + leading,
            end: end - trailing,
            text: t.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_blank_lines() {
        let md = "a\n\nb\n\nc";
        let p = split_paragraphs(md);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].text, "a");
        assert_eq!(p[2].text, "c");
    }

    #[test]
    fn handles_trailing_newlines() {
        let p = split_paragraphs("a\n\n\n\nb\n\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "a");
        assert_eq!(p[1].text, "b");
    }

    #[test]
    fn preserves_byte_offsets() {
        let md = "first.\n\nsecond.";
        let p = split_paragraphs(md);
        assert_eq!(&md[p[0].start..p[0].end], "first.");
        assert_eq!(&md[p[1].start..p[1].end], "second.");
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(split_paragraphs("").is_empty());
        assert!(split_paragraphs("   \n\n  ").is_empty());
    }
}
