//! Built-in `doc` tool — create & edit Office documents natively.
//!
//! Supported actions:
//!   - create_excel (.xlsx) via rust_xlsxwriter
//!   - create_word  (.docx) via docx-rs
//!   - create_pdf   (.pdf)  via genpdf
//!   - create_ppt   (.pptx) via zip + OOXML templates
//!   - edit_excel   (.xlsx) read existing + merge changes via calamine +
//!     rust_xlsxwriter
//!   - edit_word    (.docx) read existing + append/replace via zip XML
//!     manipulation
//!   - edit_pdf     (.pdf)  replace text / delete pages via lopdf
//!   - read_doc     (.xlsx/.docx/.pdf) extract text content for inspection

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Safe PDF text extraction (catch_unwind to handle panics in pdf-extract)
// ---------------------------------------------------------------------------

/// Extract text from PDF file path, catching panics from malformed fonts.
pub fn safe_extract_pdf_text(path: &Path) -> Result<String> {
    let path_owned = path.to_path_buf();
    match std::panic::catch_unwind(move || pdf_extract::extract_text(&path_owned)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(anyhow!("pdf extract error: {e}")),
        Err(_) => Err(anyhow!(
            "pdf extract panicked (likely malformed font encoding)"
        )),
    }
}

/// Extract text from PDF bytes in memory, catching panics.
pub fn safe_extract_pdf_from_mem(bytes: &[u8]) -> Result<String> {
    let bytes_owned = bytes.to_vec();
    match std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem(&bytes_owned)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(anyhow!("pdf extract error: {e}")),
        Err(_) => Err(anyhow!(
            "pdf extract panicked (likely malformed font encoding)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Font discovery for PDF generation
// ---------------------------------------------------------------------------

/// Try common font directories and naming patterns to find a usable font
/// family.
/// Locate a CJK-capable TTF/OTF font on disk. We try plain TTF/OTF first
/// (krilla loads them directly) before any `.ttc` collection so we don't
/// have to deal with face-index plumbing. Returns the raw bytes ready to
/// hand to `krilla::text::Font::new`.
fn find_cjk_font() -> Option<Vec<u8>> {
    let candidates: &[&str] = &[
        // macOS — plain OTF, full Han coverage
        "/Library/Fonts/AdobeSongStd-Light.otf",
        "/Library/Fonts/AdobeHeitiStd-Regular.otf",
        "/Library/Fonts/AdobeFangsongStd-Regular.otf",
        "/Library/Fonts/AdobeFanHeitiStd-Bold.otf",
        // macOS — TTC collections (krilla::Font::new takes an index; face 0
        // is the default member which is usually fine for our use case).
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        // Linux — Noto CJK is the de-facto default on most distros.
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        // Windows — CJK system fonts
        "C:\\Windows\\Fonts\\msyh.ttc",   // Microsoft YaHei (TTC)
        "C:\\Windows\\Fonts\\simsun.ttc", // SimSun
        "C:\\Windows\\Fonts\\simhei.ttf", // SimHei (plain TTF)
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn handle(args: &Value, full_path: &Path) -> Result<Value> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow!("doc: `action` required"))?
        .to_owned();

    // Auto-correct file extension based on action to prevent format mismatch
    // (e.g. model calls create_word with .txt path → fix to .docx).
    let path = {
        let expected_ext = match action.as_str() {
            "create_excel" | "edit_excel" => Some("xlsx"),
            "create_word" | "edit_word" => Some("docx"),
            "create_pdf" | "edit_pdf" => Some("pdf"),
            "create_ppt" => Some("pptx"),
            _ => None,
        };
        if let Some(ext) = expected_ext {
            let current = full_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !current.eq_ignore_ascii_case(ext) {
                full_path.with_extension(ext)
            } else {
                full_path.to_path_buf()
            }
        } else {
            full_path.to_path_buf()
        }
    };

    // All crate operations are synchronous — run on blocking pool.
    let args = args.clone();

    let result = tokio::task::spawn_blocking(move || match action.as_str() {
        "create_excel" => create_excel(&args, &path),
        "create_word" => create_word(&args, &path),
        "create_pdf" => create_pdf(&args, &path),
        "create_ppt" => create_ppt(&args, &path),
        "edit_excel" => edit_excel(&args, &path),
        "edit_word" => edit_word(&args, &path),
        "edit_pdf" => edit_pdf(&args, &path),
        "read_doc" => read_doc(&path),
        other => Ok(json!({
            "error": format!("doc: unknown action '{other}'"),
            "hint": "Valid actions: create_excel, create_word, create_pdf, create_ppt, edit_excel, edit_word, edit_pdf, read_doc. Retry with one of these.",
        })),
    })
    .await
    .map_err(|e| anyhow!("doc: spawn_blocking failed: {e}"))??;

    Ok(result)
}

// ---------------------------------------------------------------------------
// Excel (.xlsx)
// ---------------------------------------------------------------------------

fn create_excel(args: &Value, path: &Path) -> Result<Value> {
    use rust_xlsxwriter::*;

    let mut workbook = Workbook::new();

    // Styled header format: bold, background color, border, centered.
    let header_fmt = Format::new()
        .set_bold()
        .set_font_size(11.0)
        .set_background_color(Color::RGB(0x4472C4))
        .set_font_color(Color::White)
        .set_align(FormatAlign::Center)
        .set_border(FormatBorder::Thin)
        .set_border_color(Color::RGB(0x8DB4E2));

    // Data cell format: border, vertical align.
    let cell_fmt = Format::new()
        .set_font_size(10.5)
        .set_border(FormatBorder::Thin)
        .set_border_color(Color::RGB(0xD9D9D9));

    // Alternating row format.
    let alt_fmt = Format::new()
        .set_font_size(10.5)
        .set_background_color(Color::RGB(0xF2F7FB))
        .set_border(FormatBorder::Thin)
        .set_border_color(Color::RGB(0xD9D9D9));

    let sheets = args["sheets"].as_array();
    if let Some(sheets) = sheets {
        for sheet_def in sheets {
            let name = sheet_def["name"].as_str().unwrap_or("Sheet");
            let ws = workbook.add_worksheet();
            ws.set_name(name)?;

            let mut col_count = 0usize;

            // Write headers.
            if let Some(headers) = sheet_def["headers"].as_array() {
                col_count = headers.len();
                for (col, h) in headers.iter().enumerate() {
                    ws.write_string_with_format(
                        0,
                        col as u16,
                        h.as_str().unwrap_or(""),
                        &header_fmt,
                    )?;
                    // Auto-width: set column width based on header length (min 10, max 30).
                    let width = (h.as_str().unwrap_or("").len() as f64 * 1.5)
                        .max(10.0)
                        .min(30.0);
                    ws.set_column_width(col as u16, width)?;
                }
            }

            // Write data rows.
            if let Some(rows) = sheet_def["rows"].as_array() {
                for (r, row) in rows.iter().enumerate() {
                    if let Some(cells) = row.as_array() {
                        col_count = col_count.max(cells.len());
                        let fmt = if r % 2 == 0 { &cell_fmt } else { &alt_fmt };
                        for (c, cell) in cells.iter().enumerate() {
                            let row_idx = (r + 1) as u32; // +1 for header
                            let col_idx = c as u16;
                            match cell {
                                Value::Number(n) => {
                                    ws.write_number_with_format(
                                        row_idx,
                                        col_idx,
                                        n.as_f64().unwrap_or(0.0),
                                        fmt,
                                    )?;
                                }
                                Value::Bool(b) => {
                                    ws.write_boolean_with_format(row_idx, col_idx, *b, fmt)?;
                                }
                                _ => {
                                    ws.write_string_with_format(
                                        row_idx,
                                        col_idx,
                                        cell.as_str().unwrap_or(
                                            &cell.to_string().trim_matches('"').to_owned(),
                                        ),
                                        fmt,
                                    )?;
                                }
                            }
                        }
                    }
                }
            }

            // Enable auto-filter on header row.
            if col_count > 0 {
                let row_count = sheet_def["rows"].as_array().map(|r| r.len()).unwrap_or(0);
                ws.autofilter(0, 0, (row_count) as u32, (col_count - 1) as u16)?;
            }
        }
    } else {
        workbook.add_worksheet();
    }

    workbook.save(path)?;
    let sheet_count = sheets.map(|s| s.len()).unwrap_or(1);
    Ok(json!({
        "created": true,
        "path": path.display().to_string(),
        "format": "xlsx",
        "sheets": sheet_count,
    }))
}

// ---------------------------------------------------------------------------
// Word (.docx)
// ---------------------------------------------------------------------------

/// `content` without a leading heading that repeats `title`: models pass the
/// title and also open the content with `# <title>`, which printed it twice.
fn skip_repeated_title<'a>(content: &'a str, title: &str) -> &'a str {
    let title = title.trim();
    if title.is_empty() {
        return content;
    }
    let trimmed = content.trim_start();
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    if first.trim_start_matches('#').trim() == title {
        rest
    } else {
        content
    }
}

/// `text` split into (span, bold) by markdown `**` pairs. An unpaired
/// `**` is dropped rather than printed.
fn inline_spans(text: &str) -> Vec<(String, bool)> {
    let parts: Vec<&str> = text.split("**").collect();
    let paired = parts.len() % 2 == 1;
    let mut spans: Vec<(String, bool)> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        // With an odd marker count the last marker has no partner: its
        // text stays plain.
        let bold = i % 2 == 1 && (paired || i + 1 < parts.len());
        if part.is_empty() {
            continue;
        }
        match spans.last_mut() {
            Some((prev, prev_bold)) if *prev_bold == bold => prev.push_str(part),
            _ => spans.push(((*part).to_owned(), bold)),
        }
    }
    spans
}

/// `text` without markdown `**` markers (for headings, bold anyway).
fn strip_bold_markers(text: &str) -> String {
    text.replace("**", "")
}

/// Whether CJK characters make up most of the letters in `text`.
fn is_mostly_cjk(text: &str) -> bool {
    let (mut cjk, mut latin) = (0usize, 0usize);
    for c in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c) {
            cjk += 1;
        } else if c.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    cjk > 0 && cjk * 2 >= latin
}

/// A document laid out the way Chinese legal and official papers are:
/// A4 with official-document margins, a centred 宋体 title, 黑体 / 楷体
/// headings, 仿宋 body text indented two characters at a fixed line
/// height, every line its own paragraph (so each party line of a brief is
/// indented), and the closing "此致" indented, the court flush left and the
/// signature and date on the right.
fn chinese_document(title: &str, content: &str) -> docx_rs::Docx {
    use docx_rs::*;
    const LATIN: &str = "Times New Roman";
    const BODY_FONT: &str = "仿宋";
    const BODY_SIZE: usize = 28; // 14pt (四号), half-points
    const TWO_CHARS: i32 = 560; // two 14pt characters, twips
    let fonts = |east_asia: &str| RunFonts::new().east_asia(east_asia).ascii(LATIN).hi_ansi(LATIN);
    let body_spacing = || LineSpacing::new().line(520).line_rule(LineSpacingType::Exact); // 26pt
    let title_paragraph = |text: &str| {
        Paragraph::new()
            .add_run(Run::new().add_text(text).bold().size(44).fonts(fonts("宋体")))
            .align(AlignmentType::Center)
            .line_spacing(LineSpacing::new().after(240))
    };
    let heading = |text: &str, font: &str| {
        Paragraph::new()
            .add_run(Run::new().add_text(text).bold().size(BODY_SIZE).fonts(fonts(font)))
            .line_spacing(body_spacing().before(120))
    };
    // Markdown **bold** inside a line becomes real bold.
    let body_line = |text: &str| {
        inline_spans(text).into_iter().fold(Paragraph::new(), |p, (span, bold)| {
            let run = Run::new().add_text(span).size(BODY_SIZE).fonts(fonts(BODY_FONT));
            p.add_run(if bold { run.bold() } else { run })
        })
    };

    let mut docx = Docx::new()
        .page_size(11906, 16838)
        .page_margin(PageMargin::new().top(2098).bottom(1984).left(1588).right(1474));
    let mut has_title = !title.trim().is_empty();
    if has_title {
        docx = docx.add_paragraph(title_paragraph(title.trim()));
    }
    // After "此致": the next line is the court (flush left), the rest is the
    // signature block (right-aligned).
    #[derive(PartialEq)]
    enum Closing {
        No,
        Court,
        Signature,
    }
    let mut closing = Closing::No;
    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if let Some(text) = block.strip_prefix("# ") {
            // The first heading of an untitled document is its title.
            let text = strip_bold_markers(text.trim());
            docx = docx.add_paragraph(if has_title { heading(&text, "黑体") } else { title_paragraph(&text) });
            has_title = true;
            continue;
        }
        if let Some(text) = block.strip_prefix("## ") {
            docx = docx.add_paragraph(heading(&strip_bold_markers(text.trim()), "黑体"));
            continue;
        }
        if let Some(text) = block.strip_prefix("### ") {
            docx = docx.add_paragraph(heading(&strip_bold_markers(text.trim()), "楷体"));
            continue;
        }
        for line in block.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let bullet = line.strip_prefix("- ").or_else(|| line.strip_prefix("* "));
            let mut p = match bullet {
                Some(item) => body_line(&format!("• {item}")),
                None => body_line(line),
            }
            .line_spacing(body_spacing());
            if line == "此致" {
                closing = Closing::Court;
                p = p.indent(None, Some(SpecialIndentType::FirstLine(TWO_CHARS)), None, None);
            } else if closing == Closing::Court {
                closing = Closing::Signature;
            } else if closing == Closing::Signature {
                p = p.align(AlignmentType::Right);
            } else if bullet.is_none() {
                p = p.indent(None, Some(SpecialIndentType::FirstLine(TWO_CHARS)), None, None);
            }
            docx = docx.add_paragraph(p);
        }
    }
    docx
}

/// The layout for non-Chinese documents: YaHei 10.5pt, 1.5 line spacing.
fn plain_document(title: &str, content: &str) -> docx_rs::Docx {
    use docx_rs::*;
    let mut docx = Docx::new();
    let default_font = "Microsoft YaHei";
    let font_size = 21; // 10.5pt in half-points

    // Title paragraph.
    if !title.is_empty() {
        let p = Paragraph::new()
            .add_run(
                Run::new()
                    .add_text(title)
                    .bold()
                    .size(36) // 18pt
                    .fonts(RunFonts::new().east_asia(default_font)),
            )
            .style("Heading1")
            .align(AlignmentType::Center);
        docx = docx.add_paragraph(p);
        docx = docx.add_paragraph(Paragraph::new()); // blank line
    }

    // Content: split by double newlines into paragraphs.
    // Lines starting with # become headings, - or * become lists.
    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if block.starts_with("### ") {
            let text = &strip_bold_markers(&block[4..]);
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(
                        Run::new()
                            .add_text(text)
                            .bold()
                            .size(24) // 12pt
                            .fonts(RunFonts::new().east_asia(default_font)),
                    )
                    .style("Heading3"),
            );
        } else if block.starts_with("## ") {
            let text = &strip_bold_markers(&block[3..]);
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(
                        Run::new()
                            .add_text(text)
                            .bold()
                            .size(28) // 14pt
                            .fonts(RunFonts::new().east_asia(default_font)),
                    )
                    .style("Heading2"),
            );
        } else if block.starts_with("# ") {
            let text = &strip_bold_markers(&block[2..]);
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(
                        Run::new()
                            .add_text(text)
                            .bold()
                            .size(32) // 16pt
                            .fonts(RunFonts::new().east_asia(default_font)),
                    )
                    .style("Heading1"),
            );
        } else {
            // Check for list items
            let lines: Vec<&str> = block.lines().collect();
            let is_list = lines.iter().all(|l| {
                let t = l.trim();
                t.starts_with("- ") || t.starts_with("* ")
            });
            if is_list {
                for line in &lines {
                    let text = line
                        .trim()
                        .trim_start_matches("- ")
                        .trim_start_matches("* ");
                    let p = Paragraph::new().add_run(
                        Run::new()
                            .add_text(format!("  \u{2022}  {}", strip_bold_markers(text)))
                            .size(font_size)
                            .fonts(RunFonts::new().east_asia(default_font)),
                    );
                    docx = docx.add_paragraph(p);
                }
            } else {
                // Regular paragraph with line spacing. A single newline is a
                // line break: Word ignores newlines inside text, which ran a
                // brief's party lines ("答辩人…" / "住所地…" / "法定代表人…")
                // and its signature block together.
                let mut p = Paragraph::new();
                for (i, line) in block.lines().enumerate() {
                    if i > 0 {
                        p = p.add_run(Run::new().add_break(BreakType::TextWrapping));
                    }
                    for (span, bold) in inline_spans(line.trim_end()) {
                        let run = Run::new()
                            .add_text(span)
                            .size(font_size)
                            .fonts(RunFonts::new().east_asia(default_font));
                        p = p.add_run(if bold { run.bold() } else { run });
                    }
                }
                let p = p.line_spacing(LineSpacing::new().line(360)); // 1.5x line spacing
                docx = docx.add_paragraph(p);
            }
        }
    }
    docx
}

fn create_word(args: &Value, path: &Path) -> Result<Value> {

    let title = args["title"].as_str().unwrap_or("");
    let content = skip_repeated_title(args["content"].as_str().unwrap_or(""), title);

    if content.is_empty() && title.is_empty() {
        return Ok(json!({
            "error": "create_word requires 'content' parameter. Please provide the text content to write into the document.",
            "hint": "Retry with: {\"action\": \"create_word\", \"path\": \"file.docx\", \"content\": \"your text here\"}"
        }));
    }

    let docx = if is_mostly_cjk(content) || is_mostly_cjk(title) {
        chinese_document(title, content)
    } else {
        plain_document(title, content)
    };

    let file = std::fs::File::create(path)?;
    docx.build().pack(file)?;
    Ok(json!({
        "created": true,
        "path": path.display().to_string(),
        "format": "docx",
    }))
}

// ---------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------

fn create_pdf(args: &Value, path: &Path) -> Result<Value> {
    use krilla::{
        Document,
        geom::Point,
        page::PageSettings,
        text::{Font, TextDirection},
    };

    let title = args["title"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");

    if content.is_empty() && title.is_empty() {
        return Ok(json!({
            "error": "create_pdf requires 'content' parameter. Please provide the text content to write into the document.",
            "hint": "Retry with: {\"action\": \"create_pdf\", \"path\": \"file.pdf\", \"content\": \"your text here\"}"
        }));
    }

    let font_bytes = find_cjk_font().context(
        "no CJK-capable font found. Install Noto Sans CJK (Linux), \
         or ensure /Library/Fonts contains Adobe* OTF / PingFang / STHeiti (macOS), \
         or msyh.ttc / simsun.ttc / simhei.ttf (Windows)",
    )?;
    let font = Font::new(font_bytes.into(), 0)
        .ok_or_else(|| anyhow!("failed to parse the located CJK font"))?;

    // Page geometry: A4 (595 × 842 pt). krilla uses top-left origin —
    // Y grows downward — so we lay out title near the top then walk down.
    const PAGE_W: f32 = 595.0;
    const PAGE_H: f32 = 842.0;
    const MARGIN: f32 = 60.0;
    const TITLE_SIZE: f32 = 22.0;
    const BODY_SIZE: f32 = 12.0;
    const LINE_HEIGHT: f32 = 18.0;
    const TITLE_GAP: f32 = 28.0;
    const BLOCK_GAP: f32 = 10.0;
    const HEADING_PREFIX: char = '#';

    let mut document = Document::new();
    let page_size =
        PageSettings::from_wh(PAGE_W, PAGE_H).ok_or_else(|| anyhow!("invalid page size"))?;
    let mut page = document.start_page_with(page_size.clone());
    let mut surface = page.surface();
    let mut y = MARGIN;

    // Helper: start a fresh page when the cursor walked past the bottom margin.
    // krilla can't mutate `surface` and `page` across closure boundaries
    // cleanly, so we inline the check at each draw site below.
    macro_rules! ensure_page {
        ($needed:expr) => {
            if y + $needed > PAGE_H - MARGIN {
                surface.finish();
                page.finish();
                page = document.start_page_with(page_size.clone());
                surface = page.surface();
                y = MARGIN;
            }
        };
    }

    if !title.is_empty() {
        ensure_page!(TITLE_SIZE);
        // Naive centering: estimate width as TITLE_SIZE * char_count * 0.55.
        // Good enough for CJK + ASCII titles — exact centering would need
        // text shaping which we skip to stay light.
        let est_width = title.chars().count() as f32 * TITLE_SIZE * 0.55;
        let x = ((PAGE_W - est_width) / 2.0).max(MARGIN);
        surface.draw_text(
            Point::from_xy(x, y + TITLE_SIZE),
            font.clone(),
            TITLE_SIZE,
            title,
            false,
            TextDirection::Auto,
        );
        y += TITLE_SIZE + TITLE_GAP;
    }

    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let (text, size) = if let Some(stripped) = block.strip_prefix(HEADING_PREFIX) {
            (
                stripped.trim_start_matches(HEADING_PREFIX).trim(),
                BODY_SIZE + 2.0,
            )
        } else {
            (block, BODY_SIZE)
        };
        for line in text.lines() {
            ensure_page!(LINE_HEIGHT);
            surface.draw_text(
                Point::from_xy(MARGIN, y + size),
                font.clone(),
                size,
                line,
                false,
                TextDirection::Auto,
            );
            y += LINE_HEIGHT;
        }
        y += BLOCK_GAP;
    }

    surface.finish();
    page.finish();

    let bytes = document
        .finish()
        .map_err(|e| anyhow!("krilla finalize failed: {e:?}"))?;
    std::fs::write(path, &bytes)?;

    Ok(json!({
        "created": true,
        "path": path.display().to_string(),
        "format": "pdf",
        "engine": "krilla",
    }))
}

// ---------------------------------------------------------------------------
// PowerPoint (.pptx) — ZIP + OOXML
// ---------------------------------------------------------------------------

fn create_ppt(args: &Value, path: &Path) -> Result<Value> {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    let slides = args["slides"].as_array();
    let slide_data: Vec<(&str, &str)> = slides
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    (
                        s["title"].as_str().unwrap_or(""),
                        s["body"].as_str().unwrap_or(""),
                    )
                })
                .collect()
        })
        .unwrap_or_else(|| vec![("Untitled", "")]);

    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // [Content_Types].xml
    let mut ct_overrides = String::new();
    for i in 1..=slide_data.len() {
        ct_overrides.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
    }
    zip.start_file("[Content_Types].xml", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
{ct_overrides}
</Types>"#
    )?;

    // _rels/.rels
    zip.start_file("_rels/.rels", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#
    )?;

    // ppt/presentation.xml
    let mut slide_list = String::new();
    for i in 1..=slide_data.len() {
        slide_list.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            255 + i,
            i + 2
        ));
    }
    zip.start_file("ppt/presentation.xml", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst>{slide_list}</p:sldIdLst>
<p:sldSz cx="12192000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#
    )?;

    // ppt/_rels/presentation.xml.rels
    let mut pres_rels = String::new();
    pres_rels.push_str(r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#);
    pres_rels.push_str(r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="slideLayouts/slideLayout1.xml"/>"#);
    for i in 1..=slide_data.len() {
        pres_rels.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#,
            i + 2
        ));
    }
    zip.start_file("ppt/_rels/presentation.xml.rels", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{pres_rels}</Relationships>"#
    )?;

    // Minimal slide master
    zip.start_file("ppt/slideMasters/slideMaster1.xml", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#
    )?;

    zip.start_file("ppt/slideMasters/_rels/slideMaster1.xml.rels", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#
    )?;

    // Minimal slide layout
    zip.start_file("ppt/slideLayouts/slideLayout1.xml", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank">
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>
</p:sldLayout>"#
    )?;

    zip.start_file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", opts)?;
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#
    )?;

    // Individual slides
    for (i, (title, body)) in slide_data.iter().enumerate() {
        let slide_num = i + 1;
        let title_esc = xml_escape(title);
        let body_esc = xml_escape(body);

        zip.start_file(format!("ppt/slides/slide{slide_num}.xml"), opts)?;
        write!(
            zip,
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="838200" y="365125"/><a:ext cx="10515600" cy="1325563"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="3200" b="1" dirty="0"/><a:t>{title_esc}</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Content"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="838200" y="1825625"/><a:ext cx="10515600" cy="4351338"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800" dirty="0"/><a:t>{body_esc}</a:t></a:r></a:p></p:txBody></p:sp>
</p:spTree></p:cSld>
</p:sld>"#
        )?;

        zip.start_file(format!("ppt/slides/_rels/slide{slide_num}.xml.rels"), opts)?;
        write!(
            zip,
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#
        )?;
    }

    zip.finish()?;
    Ok(json!({
        "created": true,
        "path": path.display().to_string(),
        "format": "pptx",
        "slides": slide_data.len(),
    }))
}

// ---------------------------------------------------------------------------
// Edit Excel (.xlsx) — read existing + merge changes
// ---------------------------------------------------------------------------

fn edit_excel(args: &Value, path: &Path) -> Result<Value> {
    use calamine::{Data, Reader, Xlsx, open_workbook};
    use rust_xlsxwriter::*;

    let mut wb_reader: Xlsx<_> = open_workbook(path)
        .map_err(|e| {
            anyhow!(
                "edit_excel: cannot open '{}': {e}. If the file does not exist yet, use action 'create_excel' instead; otherwise verify the path with read_doc.",
                path.display()
            )
        })?;

    // Read all existing sheets into memory: Vec<(name, headers_opt, rows)>.
    let sheet_names: Vec<String> = wb_reader.sheet_names().to_vec();
    let mut existing: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for name in &sheet_names {
        let range = wb_reader
            .worksheet_range(name)
            .map_err(|e| anyhow!("edit_excel: cannot read sheet '{name}': {e}"))?;
        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|c| match c {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => f.to_string(),
                    Data::Int(i) => i.to_string(),
                    Data::Bool(b) => b.to_string(),
                    Data::DateTime(dt) => dt.to_string(),
                    Data::DateTimeIso(s) => s.clone(),
                    Data::DurationIso(s) => s.clone(),
                    Data::Error(e) => format!("{e:?}"),
                })
                .collect();
            rows.push(cells);
        }
        existing.push((name.clone(), rows));
    }

    // Build a new workbook merging existing data with edits.
    let mut workbook = Workbook::new();
    let header_fmt = Format::new().set_bold();

    // Apply "sheets" param: if a sheet name matches existing, replace it; otherwise
    // add new.
    let new_sheets = args["sheets"].as_array();
    let mut replaced: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Some(new_sheets) = new_sheets {
        for sd in new_sheets {
            let name = sd["name"].as_str().unwrap_or("Sheet");
            replaced.insert(name.to_owned());
        }
    }

    // Apply "append_rows": map sheet_name -> rows to append.
    let mut appends: std::collections::HashMap<String, Vec<Vec<Value>>> =
        std::collections::HashMap::new();
    if let Some(ar) = args["append_rows"].as_object() {
        let sheet_name = ar.get("sheet").and_then(|s| s.as_str()).unwrap_or("Sheet1");
        if let Some(rows) = ar.get("rows").and_then(|r| r.as_array()) {
            let parsed: Vec<Vec<Value>> =
                rows.iter().filter_map(|r| r.as_array().cloned()).collect();
            appends.insert(sheet_name.to_owned(), parsed);
        }
    }
    // Also support append_rows as array of {sheet, rows} objects.
    if let Some(arr) = args["append_rows"].as_array() {
        for item in arr {
            let sheet_name = item["sheet"].as_str().unwrap_or("Sheet1");
            if let Some(rows) = item["rows"].as_array() {
                let parsed: Vec<Vec<Value>> =
                    rows.iter().filter_map(|r| r.as_array().cloned()).collect();
                appends
                    .entry(sheet_name.to_owned())
                    .or_default()
                    .extend(parsed);
            }
        }
    }

    // Write existing sheets (not replaced).
    for (name, rows) in &existing {
        if replaced.contains(name) {
            continue;
        }
        let ws = workbook.add_worksheet();
        ws.set_name(name)?;
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                // Try to write as number if possible.
                if let Ok(n) = cell.parse::<f64>() {
                    ws.write_number(r as u32, c as u16, n)?;
                } else if r == 0 {
                    ws.write_string_with_format(r as u32, c as u16, cell, &header_fmt)?;
                } else {
                    ws.write_string(r as u32, c as u16, cell)?;
                }
            }
        }
        // Append rows if any.
        if let Some(extra_rows) = appends.get(name) {
            let start_row = rows.len() as u32;
            for (r, row) in extra_rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    let row_idx = start_row + r as u32;
                    let col_idx = c as u16;
                    match cell {
                        Value::Number(n) => {
                            ws.write_number(row_idx, col_idx, n.as_f64().unwrap_or(0.0))?;
                        }
                        Value::Bool(b) => {
                            ws.write_boolean(row_idx, col_idx, *b)?;
                        }
                        _ => {
                            ws.write_string(
                                row_idx,
                                col_idx,
                                cell.as_str()
                                    .unwrap_or(&cell.to_string().trim_matches('"').to_owned()),
                            )?;
                        }
                    }
                }
            }
        }
    }

    // Write replaced / new sheets from "sheets" param.
    if let Some(new_sheets) = args["sheets"].as_array() {
        for sheet_def in new_sheets {
            let name = sheet_def["name"].as_str().unwrap_or("Sheet");
            let ws = workbook.add_worksheet();
            ws.set_name(name)?;

            if let Some(headers) = sheet_def["headers"].as_array() {
                for (col, h) in headers.iter().enumerate() {
                    ws.write_string_with_format(
                        0,
                        col as u16,
                        h.as_str().unwrap_or(""),
                        &header_fmt,
                    )?;
                }
            }
            if let Some(rows) = sheet_def["rows"].as_array() {
                for (r, row) in rows.iter().enumerate() {
                    if let Some(cells) = row.as_array() {
                        for (c, cell) in cells.iter().enumerate() {
                            let row_idx = (r + 1) as u32;
                            let col_idx = c as u16;
                            match cell {
                                Value::Number(n) => {
                                    ws.write_number(row_idx, col_idx, n.as_f64().unwrap_or(0.0))?;
                                }
                                Value::Bool(b) => {
                                    ws.write_boolean(row_idx, col_idx, *b)?;
                                }
                                _ => {
                                    ws.write_string(
                                        row_idx,
                                        col_idx,
                                        cell.as_str().unwrap_or(
                                            &cell.to_string().trim_matches('"').to_owned(),
                                        ),
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Handle append_rows for sheets that were newly created via "sheets" param
    // (already written above for existing sheets).

    workbook.save(path)?;
    Ok(json!({
        "edited": true,
        "path": path.display().to_string(),
        "format": "xlsx",
    }))
}

// ---------------------------------------------------------------------------
// Zip entry size caps (zip-bomb protection)
// ---------------------------------------------------------------------------

/// Maximum uncompressed bytes read from a single zip entry.
const MAX_ZIP_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum total uncompressed bytes read from one archive.
const MAX_ZIP_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Read one zip entry fully, failing once it exceeds `MAX_ZIP_ENTRY_BYTES`
/// instead of trusting the (attacker-controlled) declared size.
fn read_zip_entry_capped<R: std::io::Read>(entry: R, name: &str) -> Result<Vec<u8>> {
    use std::io::Read as IoRead;
    let mut buf = Vec::new();
    let n = entry.take(MAX_ZIP_ENTRY_BYTES + 1).read_to_end(&mut buf)?;
    if n as u64 > MAX_ZIP_ENTRY_BYTES {
        return Err(anyhow!(
            "zip entry '{name}' exceeds {MAX_ZIP_ENTRY_BYTES} bytes uncompressed"
        ));
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Edit Word (.docx) — read existing + append/replace content
// ---------------------------------------------------------------------------

fn edit_word(args: &Value, path: &Path) -> Result<Value> {
    use std::io::Write;

    let append_text = args["append"].as_str();
    let replace_text = args["content"].as_str();

    if append_text.is_none() && replace_text.is_none() {
        return Err(anyhow!(
            "edit_word: either `append` or `content` parameter required"
        ));
    }

    if replace_text.is_some() {
        // Full replacement: just create a new doc with the given content.
        let title = args["title"].as_str().unwrap_or("");
        let mut new_args = args.clone();
        new_args["content"] =
            Value::String(replace_text.expect("replace_text checked above").to_owned());
        if !title.is_empty() {
            new_args["title"] = Value::String(title.to_owned());
        }
        create_word(&new_args, path)?;
        return Ok(json!({
            "edited": true,
            "path": path.display().to_string(),
            "format": "docx",
            "mode": "replace",
        }));
    }

    // Append mode: read existing document.xml from the zip, add paragraphs, repack.
    let append_text = append_text.expect("append_text checked above");

    let file = std::fs::File::open(path)
        .map_err(|e| {
            anyhow!(
                "edit_word: cannot open '{}': {e}. If the file does not exist yet, use action 'create_word' instead; otherwise verify the path.",
                path.display()
            )
        })?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| anyhow!("edit_word: invalid docx zip: {e}"))?;

    // Read all entries into memory, capped per entry and in total so a zip
    // bomb cannot exhaust memory.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = entry.name().to_owned();
        let buf = read_zip_entry_capped(entry, &name)?;
        total += buf.len() as u64;
        if total > MAX_ZIP_TOTAL_BYTES {
            return Err(anyhow!(
                "edit_word: docx uncompressed size exceeds {MAX_ZIP_TOTAL_BYTES} bytes"
            ));
        }
        entries.push((name, buf));
    }

    // Build new paragraphs XML to append.
    let mut new_paras = String::new();
    for block in append_text.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let escaped = xml_escape(block);
        if block.starts_with("### ") {
            let text = xml_escape(&block[4..]);
            new_paras.push_str(&format!(
                r#"<w:p><w:pPr><w:pStyle w:val="Heading3"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>{text}</w:t></w:r></w:p>"#
            ));
        } else if block.starts_with("## ") {
            let text = xml_escape(&block[3..]);
            new_paras.push_str(&format!(
                r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>{text}</w:t></w:r></w:p>"#
            ));
        } else if block.starts_with("# ") {
            let text = xml_escape(&block[2..]);
            new_paras.push_str(&format!(
                r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>{text}</w:t></w:r></w:p>"#
            ));
        } else {
            new_paras.push_str(&format!(r#"<w:p><w:r><w:t>{escaped}</w:t></w:r></w:p>"#));
        }
    }

    // Rewrite the zip with modified document.xml. Write to a temp file in
    // the same directory and rename over the original, so a failure midway
    // never leaves the user's document truncated.
    drop(archive);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document.docx".to_owned());
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let write_result = (|| -> Result<()> {
        let out_file = std::fs::File::create(&tmp_path)?;
        let mut writer = zip::ZipWriter::new(out_file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for (name, data) in &entries {
            writer.start_file(name, opts)?;
            if name == "word/document.xml" {
                // Insert new paragraphs before the closing </w:body> tag.
                let xml = String::from_utf8_lossy(data);
                if let Some(pos) = xml.rfind("</w:body>") {
                    let (before, after) = xml.split_at(pos);
                    let modified = format!("{before}{new_paras}{after}");
                    writer.write_all(modified.as_bytes())?;
                } else {
                    // Fallback: write as-is if structure not found.
                    writer.write_all(data)?;
                }
            } else {
                writer.write_all(data)?;
            }
        }
        let mut out_file = writer.finish()?;
        out_file.flush()?;
        out_file.sync_all()?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        if let Err(rm_err) = std::fs::remove_file(&tmp_path) {
            tracing::warn!(
                path = %tmp_path.display(),
                "edit_word: failed to remove temp file after error: {rm_err}"
            );
        }
        return Err(e);
    }

    Ok(json!({
        "edited": true,
        "path": path.display().to_string(),
        "format": "docx",
        "mode": "append",
    }))
}

// ---------------------------------------------------------------------------
// Edit PDF (.pdf) — replace text / delete pages via lopdf
// ---------------------------------------------------------------------------

fn edit_pdf(args: &Value, path: &Path) -> Result<Value> {
    use lopdf::{Document, Object};

    let replacements = args["replacements"].as_array();
    let delete_pages = args["delete_pages"].as_array();
    let append_content = args["content"].as_str();

    if replacements.is_none() && delete_pages.is_none() && append_content.is_none() {
        return Err(anyhow!(
            "edit_pdf: at least one of `replacements`, `delete_pages`, or `content` (append_page) is required"
        ));
    }

    let mut doc = Document::load(path)
        .map_err(|e| {
            anyhow!(
                "edit_pdf: cannot open '{}': {e}. If the file does not exist yet, use action 'create_pdf' instead; otherwise verify the path.",
                path.display()
            )
        })?;

    let mut actions_done = Vec::new();

    // --- replace_text: find/replace in page content streams ---
    if let Some(replacements) = replacements {
        let mut total_replaced = 0usize;
        let page_ids: Vec<_> = doc.page_iter().collect();

        // Collect (stream_object_ids, modified_content) pairs first to avoid borrow
        // conflicts.
        let mut updates: Vec<(Vec<lopdf::ObjectId>, Vec<u8>)> = Vec::new();

        for page_id in &page_ids {
            if let Ok(content) = doc.get_page_content(*page_id) {
                let content_str = String::from_utf8_lossy(&content).to_string();
                let mut modified = content_str.clone();

                for r in replacements {
                    let find = r["find"].as_str().unwrap_or("");
                    let replace = r["replace"].as_str().unwrap_or("");
                    if find.is_empty() {
                        continue;
                    }
                    let count = modified.matches(find).count();
                    if count > 0 {
                        modified = modified.replace(find, replace);
                        total_replaced += count;
                    }
                }

                if modified != content_str {
                    // Collect the stream object IDs from the page's Contents entry.
                    let mut stream_ids = Vec::new();
                    if let Ok(page_obj) = doc.get_object(*page_id) {
                        if let Object::Dictionary(ref dict) = *page_obj {
                            if let Ok(contents_ref) = dict.get(b"Contents") {
                                match contents_ref {
                                    Object::Reference(obj_id) => {
                                        stream_ids.push(*obj_id);
                                    }
                                    Object::Array(arr) => {
                                        for obj_ref in arr {
                                            if let Object::Reference(oid) = obj_ref {
                                                stream_ids.push(*oid);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    if !stream_ids.is_empty() {
                        updates.push((stream_ids, modified.into_bytes()));
                    }
                }
            }
        }

        // Now apply updates with mutable borrows.
        for (stream_ids, new_content) in updates {
            // Put all content in the first stream, clear the rest.
            for (i, oid) in stream_ids.iter().enumerate() {
                if let Ok(stream_obj) = doc.get_object_mut(*oid) {
                    if let Object::Stream(ref mut stream) = *stream_obj {
                        if i == 0 {
                            stream.set_plain_content(new_content.clone());
                        } else {
                            stream.set_plain_content(Vec::new());
                        }
                    }
                }
            }
        }

        actions_done.push(json!({
            "action": "replace_text",
            "replacements_applied": total_replaced,
            "note": "Text replacement works on raw PDF content streams. It may not work for text that is split across multiple TJ/Tj operators or uses font encoding."
        }));
    }

    // --- delete_pages: remove pages by 1-indexed page numbers ---
    if let Some(pages_to_delete) = delete_pages {
        let mut page_numbers: Vec<u32> = pages_to_delete
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u32))
            .collect();
        // Sort descending so we delete from the back first (indices stay valid).
        page_numbers.sort_unstable();
        page_numbers.dedup();

        let total_pages = doc.get_pages().len() as u32;
        let valid: Vec<u32> = page_numbers
            .iter()
            .copied()
            .filter(|&p| p >= 1 && p <= total_pages)
            .collect();

        if !valid.is_empty() {
            doc.delete_pages(&valid);
        }

        actions_done.push(json!({
            "action": "delete_pages",
            "deleted": valid,
            "total_pages_before": total_pages,
            "total_pages_after": total_pages as i64 - valid.len() as i64,
        }));
    }

    // --- append_page: create a new PDF with the content, save alongside ---
    if let Some(content) = append_content {
        // lopdf does not easily support creating new rendered text pages,
        // so we create a separate file with the appended content.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("doc");
        let parent = path.parent().unwrap_or(Path::new("."));
        let appended_path = parent.join(format!("{stem}_appended.pdf"));

        // Use genpdf to create the appended page.
        let append_args = serde_json::json!({
            "title": "",
            "content": content,
        });
        create_pdf(&append_args, &appended_path)?;

        actions_done.push(json!({
            "action": "append_page",
            "appended_file": appended_path.display().to_string(),
            "note": "New page saved as separate file. Use a PDF merge tool or read both files to combine. lopdf cannot easily render new text pages into an existing PDF."
        }));
    }

    // Save the modified document (replace_text and delete_pages are in-place).
    if replacements.is_some() || delete_pages.is_some() {
        doc.save(path)
            .map_err(|e| anyhow!("edit_pdf: save failed: {e}"))?;
    }

    Ok(json!({
        "edited": true,
        "path": path.display().to_string(),
        "format": "pdf",
        "actions": actions_done,
    }))
}

// ---------------------------------------------------------------------------
// Read document — extract text from xlsx/docx/pdf
// ---------------------------------------------------------------------------

fn read_doc(path: &Path) -> Result<Value> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "xlsx" | "xls" => read_excel(path),
        "docx" => read_docx(path),
        "doc" => read_legacy_doc(path),
        "pdf" => read_pdf(path),
        other => Err(anyhow!(
            "read_doc: unsupported format '.{other}'. Supported: xlsx, xls, docx, doc, pdf"
        )),
    }
}

fn read_excel(path: &Path) -> Result<Value> {
    use calamine::{Data, Reader, open_workbook_auto};

    // Auto-detect so legacy .xls opens too (the Xlsx reader rejects it).
    let mut wb = open_workbook_auto(path)
        .map_err(|e| anyhow!("read_doc: cannot open '{}': {e}", path.display()))?;

    let sheet_names: Vec<String> = wb.sheet_names().to_vec();
    let mut sheets_out = Vec::new();

    for name in &sheet_names {
        let range = wb
            .worksheet_range(name)
            .map_err(|e| anyhow!("read_doc: cannot read sheet '{name}': {e}"))?;
        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|c| match c {
                    Data::Empty => String::new(),
                    Data::String(s) => s.clone(),
                    Data::Float(f) => f.to_string(),
                    Data::Int(i) => i.to_string(),
                    Data::Bool(b) => b.to_string(),
                    Data::DateTime(dt) => dt.to_string(),
                    Data::DateTimeIso(s) => s.clone(),
                    Data::DurationIso(s) => s.clone(),
                    Data::Error(e) => format!("{e:?}"),
                })
                .collect();
            rows.push(cells);
        }
        sheets_out.push(json!({
            "name": name,
            "rows": rows,
            "row_count": rows.len(),
        }));
    }

    Ok(json!({
        "path": path.display().to_string(),
        "format": "xlsx",
        "sheets": sheets_out,
    }))
}

/// Plain text of a legacy Office file (.doc, .xls) for `read_file`, which
/// otherwise rejects them as non-UTF-8. Spreadsheet rows come back one per
/// line, cells separated by tabs, under a `# <sheet>` heading.
pub fn legacy_office_text(path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let v = match ext.as_str() {
        "doc" => read_legacy_doc(path)?,
        "xls" => read_excel(path)?,
        other => return Err(anyhow!("legacy_office_text: not a legacy Office file: .{other}")),
    };
    if let Some(text) = v["text"].as_str() {
        return Ok(text.to_owned());
    }
    let mut out = String::new();
    for sheet in v["sheets"].as_array().into_iter().flatten() {
        out.push_str(&format!("# {}\n", sheet["name"].as_str().unwrap_or("")));
        for row in sheet["rows"].as_array().into_iter().flatten() {
            let cells: Vec<&str> = row
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| c.as_str().unwrap_or(""))
                .collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
    }
    Ok(out)
}

/// Text of a legacy binary Word file (.doc). There is no pure-Rust reader,
/// so use what the system has: macOS textutil, antiword or catdoc, Word
/// itself on Windows, else LibreOffice converting to .docx.
fn read_legacy_doc(path: &Path) -> Result<Value> {
    use std::process::Command;

    let done = |text: String, via: &str| {
        json!({
            "path": path.display().to_string(),
            "format": "doc",
            "text": text,
            "length": text.len(),
            "via": via,
        })
    };
    let readers: &[(&str, &[&str])] = &[
        ("textutil", &["-convert", "txt", "-stdout"]),
        ("antiword", &["-w", "0"]),
        ("catdoc", &[]),
    ];
    for (bin, args) in readers {
        if let Ok(out) = Command::new(bin).args(*args).arg(path).output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !text.is_empty() {
                return Ok(done(text, bin));
            }
        }
    }
    #[cfg(windows)]
    {
        // Word, when installed, reads its own format; read-only, no prompts.
        let script = format!(
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $w=New-Object -ComObject Word.Application; \
             $w.Visible=$false; $d=$w.Documents.Open('{}', $false, $true); $d.Content.Text; \
             $d.Close(0); $w.Quit()",
            path.display().to_string().replace('\'', "''")
        );
        if let Ok(out) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !text.is_empty() {
                return Ok(done(text, "word"));
            }
        }
    }
    let dir = std::env::temp_dir().join(format!(
        "rsclaw-doc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    for bin in ["soffice", "libreoffice"] {
        let converted = Command::new(bin)
            .args(["--headless", "--convert-to", "docx", "--outdir"])
            .arg(&dir)
            .arg(path)
            .output();
        if converted.is_ok_and(|o| o.status.success()) {
            let docx = dir.join(path.with_extension("docx").file_name().unwrap_or_default());
            let read = read_docx(&docx);
            let _ = std::fs::remove_dir_all(&dir);
            if let Ok(mut v) = read {
                v["path"] = json!(path.display().to_string());
                v["format"] = json!("doc");
                v["via"] = json!(bin);
                return Ok(v);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Err(anyhow!(
        "read_doc: cannot read the legacy Word file '{}': no converter on this machine \
         (textutil, antiword, catdoc, Word or LibreOffice). Ask the user for a .docx or PDF copy.",
        path.display()
    ))
}

fn read_docx(path: &Path) -> Result<Value> {
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow!("read_doc: cannot open '{}': {e}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| anyhow!("read_doc: invalid docx zip: {e}"))?;

    let xml = if let Ok(entry) = archive.by_name("word/document.xml") {
        let buf = read_zip_entry_capped(entry, "word/document.xml")?;
        String::from_utf8(buf)
            .map_err(|e| anyhow!("read_doc: word/document.xml is not valid UTF-8: {e}"))?
    } else {
        return Err(anyhow!(
            "read_doc: '{}' is a zip archive but has no word/document.xml — not a valid .docx (possibly an .xlsx/.pptx or renamed file). Check the real format and retry with the matching extension, or re-create it with 'create_word'.",
            path.display()
        ));
    };

    // Extract text from <w:t> tags.
    let mut text = String::new();

    // Simple XML text extraction: find all <w:t ...>...</w:t> segments.
    let mut remaining = xml.as_str();
    while let Some(pos) = remaining.find("<w:p ").or_else(|| remaining.find("<w:p>")) {
        remaining = &remaining[pos..];
        // Find end of paragraph.
        if let Some(end) = remaining.find("</w:p>") {
            let para_xml = &remaining[..end];
            let mut para_content = String::new();
            // Extract <w:t> contents.
            let mut inner = para_xml;
            while let Some(t_start) = inner
                .find("<w:t")
                .map(|p| {
                    // Skip to after the closing > of <w:t> or <w:t ...>
                    inner[p..].find('>').map(|g| p + g + 1)
                })
                .flatten()
            {
                inner = &inner[t_start..];
                if let Some(t_end) = inner.find("</w:t>") {
                    para_content.push_str(&inner[..t_end]);
                    inner = &inner[t_end + 6..];
                } else {
                    break;
                }
            }
            if !para_content.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&para_content);
            }
            remaining = &remaining[end + 6..];
        } else {
            break;
        }
    }

    // Unescape XML entities.
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");

    Ok(json!({
        "path": path.display().to_string(),
        "format": "docx",
        "text": text,
        "length": text.len(),
    }))
}

fn read_pdf(path: &Path) -> Result<Value> {
    let text = safe_extract_pdf_text(path)
        .map_err(|e| anyhow!("read_doc: pdf extract failed for '{}': {e}", path.display()))?;

    Ok(json!({
        "path": path.display().to_string(),
        "format": "pdf",
        "text": text,
        "length": text.len(),
    }))
}

/// Escape XML special characters.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CONTENT: &str = "今天晚上7点135号168栋会议室开视频会议，参会人员：张三13800138000、李四15912345678、王五18688886666";

    #[tokio::test]
    async fn test_create_word_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_doc_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("test.docx");

        let args = json!({
            "action": "create_word",
            "path": path.to_str().expect("path to str"),
            "content": TEST_CONTENT,
            "title": "Meeting Notice"
        });
        let result = handle(&args, &path).await.expect("handle create_word");
        assert_eq!(result["created"], true, "create_word failed: {result}");
        assert!(path.exists(), "docx file not created");

        // Read back
        let read_args = json!({"action": "read_doc", "path": path.to_str().expect("path to str")});
        let read_result = handle(&read_args, &path).await.expect("handle read_doc");
        let text = read_result["text"].as_str().unwrap_or("");
        assert!(text.contains("135"), "docx missing '135': {text}");
        assert!(text.contains("168"), "docx missing '168': {text}");
        assert!(
            text.contains("13800138000"),
            "docx missing phone number: {text}"
        );

        println!("DOCX output: {}", path.display());
    }

    fn document_xml(path: &std::path::Path) -> String {
        let file = std::fs::File::open(path).expect("open docx");
        let mut zip = zip::ZipArchive::new(file).expect("zip");
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("word/document.xml").expect("document.xml"), &mut xml)
            .expect("read xml");
        xml
    }

    #[tokio::test]
    async fn create_word_lays_out_a_chinese_brief() {
        let dir = std::env::temp_dir().join("rsclaw_doc_brief_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("brief.docx");
        let content = "# 民事答辩状\n\n答辩人：武汉安和置业有限公司\n住所地：武汉市汉阳区江城大道88号\n\
            法定代表人：周宁\n\n## 一、答辩请求\n\n请求驳回被答辩人的全部诉讼请求。\n\n此致\n武汉市汉阳区人民法院\n\n\
            答辩人：武汉安和置业有限公司\n二〇二六年 月 日";
        let args = json!({"action": "create_word", "path": path.to_str().expect("path"), "content": content});
        handle(&args, &path).await.expect("create_word");
        let xml = document_xml(&path);
        assert!(!xml.contains("有限公司住所地"), "party lines ran together: {xml}");
        assert!(xml.contains("仿宋") && xml.contains("黑体") && xml.contains("Times New Roman"), "{xml}");
        assert!(xml.contains(r#"w:jc w:val="center""#), "title not centred: {xml}");
        assert!(xml.contains(r#"w:firstLine="560""#), "body not indented two characters: {xml}");
        // Signature and date on the right; the court flush left.
        assert_eq!(xml.matches(r#"w:jc w:val="right""#).count(), 2, "{xml}");
        assert!(xml.contains(r#"w:top="2098""#), "official margins missing: {xml}");
    }

    #[test]
    fn inline_spans_turn_markdown_bold_into_bold() {
        assert_eq!(
            inline_spans("**被答辩人（原告）：陈东**，男"),
            vec![("被答辩人（原告）：陈东".to_owned(), true), ("，男".to_owned(), false)]
        );
        assert_eq!(inline_spans("plain"), vec![("plain".to_owned(), false)]);
        // An unpaired marker is dropped, its text stays plain.
        assert_eq!(inline_spans("a **b"), vec![("a b".to_owned(), false)]);
    }

    #[tokio::test]
    async fn create_word_prints_no_markdown_markers() {
        let dir = std::env::temp_dir().join("rsclaw_doc_bold_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        for (name, content) in [
            ("zh.docx", "## **一、答辩请求**\n\n**答辩人（被告）：武汉安和置业有限公司**\n住所地：武汉"),
            ("en.docx", "## **Claims**\n\nThe **defendant** denies it."),
        ] {
            let path = dir.join(name);
            let args = json!({"action": "create_word", "path": path.to_str().expect("path"), "content": content});
            handle(&args, &path).await.expect("create_word");
            let xml = document_xml(&path);
            assert!(!xml.contains("**"), "{name}: markers printed: {xml}");
            assert!(xml.contains("<w:b />") || xml.contains("<w:b/>"), "{name}: no bold run: {xml}");
        }
    }

    #[tokio::test]
    async fn create_word_keeps_line_breaks_in_plain_documents() {
        let dir = std::env::temp_dir().join("rsclaw_doc_linebreak_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("notes.docx");
        let args = json!({
            "action": "create_word",
            "path": path.to_str().expect("path"),
            "content": "Attendees: Alice\nBob\nCarol\n\nNext meeting on Monday",
        });
        handle(&args, &path).await.expect("create_word");
        assert_eq!(document_xml(&path).matches("<w:br").count(), 2);
    }

    #[tokio::test]
    async fn test_create_pdf_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_pdf_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("test.pdf");

        let args = json!({
            "action": "create_pdf",
            "path": path.to_str().expect("path to str"),
            "content": TEST_CONTENT,
            "title": "Meeting Notice"
        });
        let result = handle(&args, &path).await.expect("handle create_pdf");
        assert_eq!(result["created"], true, "create_pdf failed: {result}");
        assert!(path.exists(), "pdf file not created");
        assert!(
            path.metadata().expect("pdf metadata").len() > 0,
            "pdf file is empty"
        );

        // Read back via read_doc
        let read_args = json!({"action": "read_doc", "path": path.to_str().expect("path to str")});
        let read_result = handle(&read_args, &path).await.expect("handle read_doc");
        let text = read_result["text"].as_str().unwrap_or("");
        println!("PDF read_doc text: '{text}'");
        // PDF text extraction may have spacing differences, just check numbers exist
        assert!(
            text.contains("135") || text.contains("1 3 5"),
            "pdf missing '135': {text}"
        );
        assert!(
            text.contains("13800138000") || text.contains("1380013800"),
            "pdf missing phone: {text}"
        );

        println!("PDF output: {}", path.display());
    }

    #[tokio::test]
    async fn test_create_excel_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_xlsx_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("test.xlsx");

        let args = json!({
            "action": "create_excel",
            "path": path.to_str().expect("path to str"),
            "sheets": [{"name": "Sheet1", "headers": ["Name","Phone"], "rows": [["Zhang","13800138000"],["Li","15912345678"]]}]
        });
        let result = handle(&args, &path).await.expect("handle create_excel");
        assert_eq!(result["created"], true, "create_excel failed: {result}");
        assert!(path.exists(), "xlsx file not created");

        // Read back — xlsx returns {"sheets": [{"rows": [...]}]}, not "text"
        let read_args = json!({"action": "read_doc", "path": path.to_str().expect("path to str")});
        let read_result = handle(&read_args, &path).await.expect("handle read_doc");
        let sheets_json = serde_json::to_string(&read_result["sheets"]).unwrap_or_default();
        assert!(
            sheets_json.contains("13800138000"),
            "xlsx missing phone: {sheets_json}"
        );

        // Don't clean up — let user inspect files
        println!("XLSX output: {}", path.display());
    }

    #[tokio::test]
    async fn test_create_word_empty_content_rejected() {
        let dir = std::env::temp_dir().join("rsclaw_empty_test");
        std::fs::create_dir_all(&dir).expect("create test dir");
        let path = dir.join("empty.docx");

        let args = json!({"action": "create_word", "path": path.to_str().expect("path to str")});
        let result = handle(&args, &path)
            .await
            .expect("handle create_word empty");
        assert!(
            result.get("error").is_some(),
            "empty content should return error: {result}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod skip_repeated_title_tests {
    use super::skip_repeated_title;

    #[test]
    fn drops_only_a_heading_that_repeats_the_title() {
        assert_eq!(skip_repeated_title("# 民事答辩状\n\n答辩人：甲", "民事答辩状"), "\n答辩人：甲");
        assert_eq!(skip_repeated_title("民事答辩状\n答辩人：甲", "民事答辩状"), "答辩人：甲");
        assert_eq!(skip_repeated_title("# 答辩请求\n一、", "民事答辩状"), "# 答辩请求\n一、");
        assert_eq!(skip_repeated_title("# 民事答辩状", ""), "# 民事答辩状");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod legacy_doc_tests {
    #[test]
    fn reads_a_legacy_word_file() {
        let dir = std::env::temp_dir().join(format!("rsclaw-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("note.txt");
        std::fs::write(&txt, "证据目录\n证据一：中标结果公告").unwrap();
        let doc = dir.join("note.doc");
        let made = std::process::Command::new("textutil")
            .args(["-convert", "doc", "-output"])
            .arg(&doc)
            .arg(&txt)
            .status()
            .unwrap();
        assert!(made.success());
        let v = super::read_legacy_doc(&doc).unwrap();
        assert_eq!(v["format"], "doc");
        assert!(v["text"].as_str().unwrap().contains("中标结果公告"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
