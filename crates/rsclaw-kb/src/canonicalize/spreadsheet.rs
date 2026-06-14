//! Spreadsheet canonicalizer (xlsx / xls / ods / xlsb) via `calamine`.
//!
//! Replaces the earlier hand-rolled xlsx extractor that read only
//! `xl/sharedStrings.xml` — that dropped inline-string and numeric-only
//! sheets entirely (reported as "unsupported or empty content"). calamine
//! walks the worksheet grid, so it captures cell text AND numbers, and it
//! auto-detects the workbook format, which gets us legacy `.xls` and
//! OpenDocument `.ods` for free.

use std::io::Cursor;

use calamine::{Data, Reader, open_workbook_auto_from_rs};

use super::*;
use crate::content_store::atomic::sha256_hex;

pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const XLS_MIME: &str = "application/vnd.ms-excel";
pub const ODS_MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";

pub struct SpreadsheetCanonicalizer;

impl SpreadsheetCanonicalizer {
    fn cell_to_string(c: &Data) -> String {
        match c {
            Data::Empty => String::new(),
            Data::String(s) => s.clone(),
            Data::Float(f) => f.to_string(),
            Data::Int(i) => i.to_string(),
            Data::Bool(b) => b.to_string(),
            Data::DateTime(dt) => dt.to_string(),
            Data::DateTimeIso(s) => s.clone(),
            Data::DurationIso(s) => s.clone(),
            Data::Error(e) => format!("{e:?}"),
        }
    }
}

impl Canonicalizer for SpreadsheetCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, XLSX_MIME | XLS_MIME | ODS_MIME)
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let mut wb = open_workbook_auto_from_rs(Cursor::new(input.bytes))
            .map_err(|e| anyhow::anyhow!("not a readable spreadsheet: {e}"))?;

        let sheet_names = wb.sheet_names().to_vec();
        let mut sections = Vec::new();
        let mut total_rows = 0usize;

        for name in &sheet_names {
            let range = match wb.worksheet_range(name) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(sheet = %name, "spreadsheet: skipping unreadable sheet: {e}");
                    continue;
                }
            };
            let mut lines = Vec::new();
            for row in range.rows() {
                let mut cells: Vec<String> = row.iter().map(Self::cell_to_string).collect();
                // Drop trailing empty cells so a sparse grid doesn't emit a
                // wall of separators; skip rows that are entirely empty.
                while cells.last().is_some_and(|c| c.is_empty()) {
                    cells.pop();
                }
                if cells.is_empty() {
                    continue;
                }
                lines.push(cells.join(" | "));
            }
            if lines.is_empty() {
                continue;
            }
            total_rows += lines.len();
            sections.push(format!("## {name}\n\n{}", lines.join("\n")));
        }

        // Genuinely empty workbook (no rows anywhere) → not an error, just
        // nothing to index.
        if sections.is_empty() {
            return Ok(None);
        }

        let md = sections.join("\n\n");
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        let extra = serde_json::json!({
            "n_sheets": sheet_names.len(),
            "n_rows": total_rows,
        });
        Ok(Some(CanonicalizedSource {
            markdown: md,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Untitled.xlsx").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use rust_xlsxwriter::Workbook;

    use super::*;

    fn input<'a>(bytes: &'a [u8], mime: &'a str) -> CanonicalizeInput<'a> {
        CanonicalizeInput {
            bytes,
            mime,
            hint_title: Some("book.xlsx"),
            logical_source_id_seed: None,
        }
    }

    /// A real xlsx (built with rust_xlsxwriter) with both strings and numbers.
    /// The old shared-strings-only extractor would have lost the numbers; this
    /// proves calamine captures the full grid.
    fn build_xlsx() -> Vec<u8> {
        let mut wb = Workbook::new();
        let s = wb.add_worksheet();
        s.write_string(0, 0, "姓名").unwrap();
        s.write_string(0, 1, "分数").unwrap();
        s.write_string(1, 0, "Alice").unwrap();
        s.write_number(1, 1, 95.0).unwrap();
        s.write_string(2, 0, "Bob").unwrap();
        s.write_number(2, 1, 88.0).unwrap();
        wb.save_to_buffer().unwrap()
    }

    #[test]
    fn xlsx_captures_strings_and_numbers() {
        let bytes = build_xlsx();
        let out = SpreadsheetCanonicalizer
            .canonicalize(input(&bytes, XLSX_MIME))
            .unwrap()
            .expect("some");
        assert!(
            out.markdown.contains("姓名 | 分数"),
            "got: {}",
            out.markdown
        );
        assert!(out.markdown.contains("Alice | 95"), "got: {}", out.markdown);
        assert!(out.markdown.contains("Bob | 88"), "got: {}", out.markdown);
    }

    #[test]
    fn empty_workbook_is_none() {
        let mut wb = Workbook::new();
        wb.add_worksheet();
        let bytes = wb.save_to_buffer().unwrap();
        let out = SpreadsheetCanonicalizer
            .canonicalize(input(&bytes, XLSX_MIME))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn non_spreadsheet_bytes_is_error() {
        let r = SpreadsheetCanonicalizer.canonicalize(input(b"not a workbook", XLSX_MIME));
        assert!(r.is_err());
    }
}
