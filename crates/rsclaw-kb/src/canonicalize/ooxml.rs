//! OOXML (docx / xlsx / pptx) → markdown/text canonicalizers.
//!
//! Office Open XML files are zip archives of XML parts. Rather than depend
//! on a high-level office crate, we unzip the relevant part(s) and pull the
//! text out of the known text-bearing elements:
//!   - docx: `word/document.xml`, text in `<w:t>`, paragraphs `<w:p>`
//!   - pptx: `ppt/slides/slideN.xml`, text in `<a:t>`, one section per slide
//!   - xlsx: `xl/sharedStrings.xml`, cell strings in `<t>`
//!
//! This loses fine layout (exact table grids, run formatting) but preserves
//! all the text content, which is what the KB embeds and searches over.

use std::io::{Cursor, Read};

use super::*;
use crate::content_store::atomic::sha256_hex;

/// Extract the inner text of every `<tag ...>...</tag>` occurrence in `xml`,
/// in document order, with the basic XML entities decoded. Empty captures are
/// kept (callers decide how to join); whitespace is preserved verbatim
/// (OOXML uses `xml:space="preserve"` for significant spaces).
fn extract_tag_text(xml: &str, tag: &str) -> Vec<String> {
    // `<tag>` or `<tag attr...>` ... `</tag>`, non-greedy, dotall. The
    // `(?:\s[^>]*)?` guard requires the char after the tag name to be `>` or
    // whitespace, so `<w:tab/>` is not mistaken for `<w:t>`.
    let t = regex::escape(tag);
    let pat = format!(r"(?s)<{t}(?:\s[^>]*)?>(.*?)</{t}>");
    let re = regex::Regex::new(&pat).expect("static OOXML tag regex is valid");
    re.captures_iter(xml)
        .map(|c| decode_xml_entities(&c[1]))
        .collect()
}

/// Decode the five predefined XML entities. `&amp;` is decoded last so a
/// literal `&amp;lt;` round-trips to `&lt;` rather than `<`. (Numeric
/// entities are rare in office text bodies; left as-is for v1.)
fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub const DOCX_MIME: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX_MIME: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";

/// Read one entry from a zip archive. `Ok(None)` if the entry is absent
/// (e.g. an xlsx with no shared-string table); `Err` if the bytes aren't a
/// readable zip at all.
fn read_zip_part(bytes: &[u8], name: &str) -> Result<Option<String>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("not a valid OOXML (zip) file: {e}"))?;
    match archive.by_name(name) {
        Ok(mut f) => {
            let mut s = String::new();
            f.read_to_string(&mut s)?;
            Ok(Some(s))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {name} from zip: {e}")),
    }
}

fn make_source(
    input: &CanonicalizeInput<'_>,
    markdown: String,
    default_title: &str,
    extra: serde_json::Value,
) -> CanonicalizedSource {
    let lsid = input
        .logical_source_id_seed
        .clone()
        .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
    CanonicalizedSource {
        markdown,
        metadata: CanonicalMetadata {
            source_kind: KbSourceKind::Doc,
            logical_source_id: lsid,
            title: input.hint_title.unwrap_or(default_title).to_string(),
            mime: input.mime.to_string(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            tags: vec![],
            extra,
        },
    }
}

// --- docx ----------------------------------------------------------------

pub struct DocxCanonicalizer;

impl Canonicalizer for DocxCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }
    fn supports_mime(&self, mime: &str) -> bool {
        mime == DOCX_MIME
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let xml = match read_zip_part(input.bytes, "word/document.xml")? {
            Some(x) => x,
            None => return Ok(None),
        };
        // Each `<w:p>` is a paragraph; its visible text is the concatenation
        // of the `<w:t>` runs inside it. Splitting on `</w:p>` keeps paragraph
        // boundaries (→ blank line in markdown).
        let mut paras = Vec::new();
        for seg in xml.split("</w:p>") {
            let runs = extract_tag_text(seg, "w:t");
            if runs.is_empty() {
                continue;
            }
            let para = runs.join("");
            let trimmed = para.trim();
            if !trimmed.is_empty() {
                paras.push(trimmed.to_string());
            }
        }
        if paras.is_empty() {
            return Ok(None);
        }
        let md = paras.join("\n\n");
        let extra = serde_json::json!({ "n_paragraphs": paras.len() });
        Ok(Some(make_source(&input, md, "Untitled.docx", extra)))
    }
}

// xlsx now lives in `spreadsheet.rs` (calamine-based; also handles .xls/.ods).

// --- pptx ----------------------------------------------------------------

pub struct PptxCanonicalizer;

fn slide_number(name: &str) -> u32 {
    name.rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(0)
}

impl Canonicalizer for PptxCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }
    fn supports_mime(&self, mime: &str) -> bool {
        mime == PPTX_MIME
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let mut archive = zip::ZipArchive::new(Cursor::new(input.bytes))
            .map_err(|e| anyhow::anyhow!("not a valid pptx (zip) file: {e}"))?;
        // Collect slide part names first (releases the immutable borrow before
        // the mutable `by_name` reads below), then read in slide order.
        let mut slides: Vec<String> = archive
            .file_names()
            .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
            .map(|s| s.to_string())
            .collect();
        slides.sort_by_key(|n| slide_number(n));
        let mut sections = Vec::new();
        for (i, name) in slides.iter().enumerate() {
            let mut xml = String::new();
            archive.by_name(name)?.read_to_string(&mut xml)?;
            let runs: Vec<String> = extract_tag_text(&xml, "a:t")
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if runs.is_empty() {
                continue;
            }
            sections.push(format!("## Slide {}\n\n{}", i + 1, runs.join("\n")));
        }
        if sections.is_empty() {
            return Ok(None);
        }
        let md = sections.join("\n\n");
        let extra = serde_json::json!({ "n_slides": slides.len() });
        Ok(Some(make_source(&input, md, "Untitled.pptx", extra)))
    }
}

#[cfg(test)]
mod canon_tests {
    use std::io::Write;

    use super::*;

    fn make_zip(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default();
            for (name, content) in parts {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(content.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    fn input<'a>(bytes: &'a [u8], mime: &'a str) -> CanonicalizeInput<'a> {
        CanonicalizeInput {
            bytes,
            mime,
            hint_title: Some("t"),
            logical_source_id_seed: None,
        }
    }

    #[test]
    fn docx_extracts_paragraphs() {
        let doc = "<w:document><w:body>\
            <w:p><w:t>第一段中文</w:t></w:p>\
            <w:p><w:t>second </w:t><w:t>paragraph</w:t></w:p>\
            </w:body></w:document>";
        let bytes = make_zip(&[("word/document.xml", doc)]);
        let out = DocxCanonicalizer
            .canonicalize(input(&bytes, DOCX_MIME))
            .unwrap()
            .expect("some");
        assert_eq!(out.markdown, "第一段中文\n\nsecond paragraph");
    }

    #[test]
    fn pptx_extracts_slides_in_order() {
        // Insert slide2 before slide1 to prove numeric ordering, not zip order.
        let s2 = "<p:sld><a:t>第二页</a:t></p:sld>";
        let s1 = "<p:sld><a:t>第一页</a:t><a:t>标题</a:t></p:sld>";
        let bytes = make_zip(&[("ppt/slides/slide2.xml", s2), ("ppt/slides/slide1.xml", s1)]);
        let out = PptxCanonicalizer
            .canonicalize(input(&bytes, PPTX_MIME))
            .unwrap()
            .expect("some");
        assert_eq!(
            out.markdown,
            "## Slide 1\n\n第一页\n标题\n\n## Slide 2\n\n第二页"
        );
    }

    #[test]
    fn invalid_zip_is_error_not_panic() {
        let r = DocxCanonicalizer.canonicalize(input(b"not a zip", DOCX_MIME));
        assert!(r.is_err());
    }

    #[test]
    fn docx_with_no_text_is_none() {
        let bytes = make_zip(&[(
            "word/document.xml",
            "<w:document><w:body></w:body></w:document>",
        )]);
        let out = DocxCanonicalizer
            .canonicalize(input(&bytes, DOCX_MIME))
            .unwrap();
        assert!(out.is_none());
    }
}

#[cfg(test)]
mod extract_tests {
    use super::*;

    #[test]
    fn pulls_text_in_order() {
        let xml = "<w:p><w:t>Hello</w:t><w:t> world</w:t></w:p>";
        assert_eq!(extract_tag_text(xml, "w:t"), vec!["Hello", " world"]);
    }

    #[test]
    fn ignores_attributes_and_preserves_space() {
        let xml = r#"<w:t xml:space="preserve">leading </w:t>"#;
        assert_eq!(extract_tag_text(xml, "w:t"), vec!["leading "]);
    }

    #[test]
    fn decodes_basic_entities() {
        let xml = "<a:t>A &amp; B &lt;tag&gt; &quot;q&quot;</a:t>";
        assert_eq!(extract_tag_text(xml, "a:t"), vec![r#"A & B <tag> "q""#]);
    }

    #[test]
    fn does_not_match_other_tags_or_substrings() {
        // `<w:tab/>` must not be picked up as a `<w:t>`.
        let xml = "<w:tab/><w:t>real</w:t><w:rPr>x</w:rPr>";
        assert_eq!(extract_tag_text(xml, "w:t"), vec!["real"]);
    }
}
