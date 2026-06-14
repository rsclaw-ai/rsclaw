//! MIME detection + dispatch to the right canonicalizer.

use super::*;
use crate::canonicalize::{
    email::{EML_MIME, EmlCanonicalizer, MBOX_MIME, MboxCanonicalizer},
    html::HtmlCanonicalizer,
    image::ImageCanonicalizer,
    legacy::{DOC_MIME, LegacyOfficeCanonicalizer, PPT_MIME},
    md::MdCanonicalizer,
    ooxml::{DOCX_MIME, DocxCanonicalizer, PPTX_MIME, PptxCanonicalizer},
    pdf::PdfCanonicalizer,
    spreadsheet::{ODS_MIME, SpreadsheetCanonicalizer, XLS_MIME, XLSX_MIME},
    text::TextCanonicalizer,
};

/// Detect MIME from byte magic + filename hint. Conservative: returns
/// `application/octet-stream` when in doubt (so `canonicalize_by_mime`
/// will error rather than guess).
pub fn detect_mime(bytes: &[u8], filename_hint: Option<&str>) -> String {
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf".into();
    }
    // Image magic bytes (OCR canonicalizer handles these).
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".into();
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif".into();
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
            // Spreadsheets calamine can read directly (legacy Excel + ODF).
            "xls" => return XLS_MIME.into(),
            "ods" => return ODS_MIME.into(),
            // Legacy binary Word/PowerPoint: detected so we can return a
            // clear "save as .docx/.pptx" message instead of a generic error.
            "doc" => return DOC_MIME.into(),
            "ppt" => return PPT_MIME.into(),
            "eml" => return EML_MIME.into(),
            "mbox" => return MBOX_MIME.into(),
            "png" => return "image/png".into(),
            "jpg" | "jpeg" => return "image/jpeg".into(),
            "webp" => return "image/webp".into(),
            "gif" => return "image/gif".into(),
            "bmp" => return "image/bmp".into(),
            "tif" | "tiff" => return "image/tiff".into(),
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

pub fn canonicalize_by_mime(input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
    let registered: &[&dyn Canonicalizer] = &[
        &MdCanonicalizer,
        &HtmlCanonicalizer,
        &ImageCanonicalizer,
        &PdfCanonicalizer,
        &TextCanonicalizer,
        &DocxCanonicalizer,
        &SpreadsheetCanonicalizer,
        &PptxCanonicalizer,
        &LegacyOfficeCanonicalizer,
        &EmlCanonicalizer,
        &MboxCanonicalizer,
    ];
    for c in registered {
        if c.supports_mime(input.mime) {
            return c.canonicalize(input);
        }
    }
    Err(anyhow::anyhow!(
        "no canonicalizer for mime '{}'",
        input.mime
    ))
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
        // Email: routed by extension (RFC822 has no reliable magic).
        assert_eq!(detect_mime(b"From: a@b\r\n", Some("msg.eml")), EML_MIME);
        assert_eq!(detect_mime(b"From a@b\r\n", Some("inbox.mbox")), MBOX_MIME);
        // Legacy/ODF spreadsheets and binary Office (OLE2 magic, by extension).
        assert_eq!(detect_mime(b"\xd0\xcf\x11\xe0", Some("old.xls")), XLS_MIME);
        assert_eq!(detect_mime(b"PK\x03\x04", Some("calc.ods")), ODS_MIME);
        assert_eq!(detect_mime(b"\xd0\xcf\x11\xe0", Some("old.doc")), DOC_MIME);
        assert_eq!(detect_mime(b"\xd0\xcf\x11\xe0", Some("old.ppt")), PPT_MIME);
    }

    #[test]
    fn legacy_doc_ppt_give_actionable_error() {
        for (mime, want) in [(DOC_MIME, ".docx"), (PPT_MIME, ".pptx")] {
            let err = canonicalize_by_mime(CanonicalizeInput {
                bytes: b"\xd0\xcf\x11\xe0junk",
                mime,
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains(want), "error should suggest {want}: {err}");
        }
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
