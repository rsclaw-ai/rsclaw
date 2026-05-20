//! MIME detection + dispatch to the right canonicalizer.

use super::*;
use crate::kb::canonicalize::{
    html::HtmlCanonicalizer, md::MdCanonicalizer,
    ooxml::{
        DocxCanonicalizer, PptxCanonicalizer, XlsxCanonicalizer, DOCX_MIME, PPTX_MIME, XLSX_MIME,
    },
    pdf::PdfCanonicalizer, text::TextCanonicalizer,
};

/// Detect MIME from byte magic + filename hint. Conservative: returns
/// `application/octet-stream` when in doubt (so `canonicalize_by_mime`
/// will error rather than guess).
pub fn detect_mime(bytes: &[u8], filename_hint: Option<&str>) -> String {
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf".into();
    }
    if let Some(name) = filename_hint {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => return "text/markdown".into(),
            "html" | "htm" => return "text/html".into(),
            "pdf" => return "application/pdf".into(),
            "txt" | "log" => return "text/plain".into(),
            "csv" => return "text/csv".into(),
            // OOXML are all zip (PK) by magic; the extension is what
            // distinguishes Word / Excel / PowerPoint.
            "docx" => return DOCX_MIME.into(),
            "xlsx" => return XLSX_MIME.into(),
            "pptx" => return PPTX_MIME.into(),
            _ => {}
        }
    }
    // Crude ASCII sniff: first 512 bytes are printable / whitespace → plain text.
    if bytes
        .iter()
        .take(512)
        .all(|b| *b == b'\n' || *b == b'\r' || *b == b'\t' || (*b >= 0x20 && *b < 0x7f))
    {
        return "text/plain".into();
    }
    "application/octet-stream".into()
}

pub fn canonicalize_by_mime(
    input: CanonicalizeInput<'_>,
) -> Result<Option<CanonicalizedSource>> {
    let registered: &[&dyn Canonicalizer] = &[
        &MdCanonicalizer,
        &HtmlCanonicalizer,
        &PdfCanonicalizer,
        &TextCanonicalizer,
        &DocxCanonicalizer,
        &XlsxCanonicalizer,
        &PptxCanonicalizer,
    ];
    for c in registered {
        if c.supports_mime(input.mime) {
            return c.canonicalize(input);
        }
    }
    Err(anyhow::anyhow!("no canonicalizer for mime '{}'", input.mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_pdf_magic() {
        assert_eq!(detect_mime(b"%PDF-1.5\n", None), "application/pdf");
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect_mime(b"# x", Some("a.md")), "text/markdown");
        assert_eq!(detect_mime(b"<", Some("a.html")), "text/html");
        assert_eq!(detect_mime(b"x", Some("a.txt")), "text/plain");
        // OOXML: binary zip bytes, routed purely by extension.
        assert_eq!(detect_mime(b"PK\x03\x04", Some("report.docx")), DOCX_MIME);
        assert_eq!(detect_mime(b"PK\x03\x04", Some("sheet.xlsx")), XLSX_MIME);
        assert_eq!(detect_mime(b"PK\x03\x04", Some("deck.pptx")), PPTX_MIME);
    }

    #[test]
    fn dispatch_routes_to_md() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"# Hi\nbody",
            mime: "text/markdown",
            hint_title: None,
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(r.metadata.title, "Hi");
    }

    #[test]
    fn unknown_mime_errors() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"x",
            mime: "application/x-unknown",
            hint_title: None,
            logical_source_id_seed: None,
        });
        assert!(r.is_err());
    }
}
