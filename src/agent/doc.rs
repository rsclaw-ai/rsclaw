//! Built-in `doc` tool — create & edit Office documents natively.
//!
//! Supported actions:
//!   - create_excel (.xlsx) via rust_xlsxwriter
//!   - create_word  (.docx) via docx-rs
//!   - create_pdf   (.pdf)  via genpdf
//!   - create_ppt   (.pptx) via zip + OOXML templates
//!   - edit_excel   (.xlsx) read existing + merge changes via calamine + rust_xlsxwriter
//!   - edit_word    (.docx) read existing + append/replace via zip XML manipulation
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
        Err(_) => Err(anyhow!("pdf extract panicked (likely malformed font encoding)")),
    }
}

/// Extract text from PDF bytes in memory, catching panics.
pub fn safe_extract_pdf_from_mem(bytes: &[u8]) -> Result<String> {
    let bytes_owned = bytes.to_vec();
    match std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem(&bytes_owned)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(anyhow!("pdf extract error: {e}")),
        Err(_) => Err(anyhow!("pdf extract panicked (likely malformed font encoding)")),
    }
}

// ---------------------------------------------------------------------------
// Font discovery for PDF generation
// ---------------------------------------------------------------------------

/// Try common font directories and naming patterns to find a usable font family.
fn find_font_family() -> Option<genpdf::fonts::FontFamily<genpdf::fonts::FontData>> {
    // Directories and font names to try, in order.
    let candidates: &[(&str, &str)] = &[
        // Linux
        ("/usr/share/fonts/truetype/liberation", "LiberationSans"),
        ("/usr/share/fonts/truetype/dejavu", "DejaVuSans"),
        // macOS Supplemental (TTF, genpdf-compatible naming with space→hyphen)
        // genpdf looks for {Name}-Regular.ttf so we need to create symlinks or
        // load manually. Instead, try directories where fonts follow the pattern.
        ("/usr/share/fonts", "LiberationSans"),
    ];
    for (dir, name) in candidates {
        if let Ok(f) = genpdf::fonts::from_files(dir, name, None) {
            return Some(f);
        }
    }
    // macOS: load individual TTF files directly (genpdf fallback only, Chrome preferred).
    let mac_fonts: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
        "/System/Library/Fonts/Geneva.ttf",
    ];
    // Windows: CJK fonts for genpdf fallback
    let win_fonts: &[&str] = &[
        "C:\\Windows\\Fonts\\simhei.ttf",   // SimHei (TTF, CJK)
    ];
    for path in mac_fonts {
        if let Ok(data) = std::fs::read(path) {
            if let Ok(fd) = genpdf::fonts::FontData::new(data, None) {
                return Some(genpdf::fonts::FontFamily {
                    regular: fd.clone(),
                    bold: fd.clone(),
                    italic: fd.clone(),
                    bold_italic: fd,
                });
            }
        }
    }
    // Windows: try CJK fonts first
    for path in win_fonts {
        if let Ok(data) = std::fs::read(path) {
            if let Ok(fd) = genpdf::fonts::FontData::new(data, None) {
                return Some(genpdf::fonts::FontFamily {
                    regular: fd.clone(),
                    bold: fd.clone(),
                    italic: fd.clone(),
                    bold_italic: fd,
                });
            }
        }
    }
    // Windows: Latin fallback
    if let Ok(f) = genpdf::fonts::from_files("C:\\Windows\\Fonts", "arial", None) {
        return Some(f);
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
        other => Ok(json!({"error": format!("doc: unknown action '{other}'")})),
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
                    let width = (h.as_str().unwrap_or("").len() as f64 * 1.5).max(10.0).min(30.0);
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
                                        cell.as_str()
                                            .unwrap_or(&cell.to_string().trim_matches('"').to_owned()),
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

fn create_word(args: &Value, path: &Path) -> Result<Value> {
    use docx_rs::*;

    let title = args["title"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");

    if content.is_empty() && title.is_empty() {
        return Ok(json!({
            "error": "create_word requires 'content' parameter. Please provide the text content to write into the document.",
            "hint": "Retry with: {\"action\": \"create_word\", \"path\": \"file.docx\", \"content\": \"your text here\"}"
        }));
    }

    let mut docx = Docx::new();

    // Default font: use CJK-friendly font stack.
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
            let text = &block[4..];
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
            let text = &block[3..];
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
            let text = &block[2..];
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
                    let text = line.trim().trim_start_matches("- ").trim_start_matches("* ");
                    let p = Paragraph::new()
                        .add_run(
                            Run::new()
                                .add_text(format!("  \u{2022}  {text}"))
                                .size(font_size)
                                .fonts(RunFonts::new().east_asia(default_font)),
                        );
                    docx = docx.add_paragraph(p);
                }
            } else {
                // Regular paragraph with line spacing.
                let p = Paragraph::new()
                    .add_run(
                        Run::new()
                            .add_text(block)
                            .size(font_size)
                            .fonts(RunFonts::new().east_asia(default_font)),
                    )
                    .line_spacing(LineSpacing::new().line(360)); // 1.5x line spacing
                docx = docx.add_paragraph(p);
            }
        }
    }

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
    let title = args["title"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");

    if content.is_empty() && title.is_empty() {
        return Ok(json!({
            "error": "create_pdf requires 'content' parameter. Please provide the text content to write into the document.",
            "hint": "Retry with: {\"action\": \"create_pdf\", \"path\": \"file.pdf\", \"content\": \"your text here\"}"
        }));
    }

    // Strategy: generate HTML then convert to PDF via Chrome headless (best CJK support).
    // Fallback to genpdf if Chrome is not available.
    let html = build_html_for_pdf(title, content);

    // Try Chrome headless first (supports CJK natively via system fonts).
    if let Some(chrome) = crate::agent::platform::detect_chrome() {
        let tmp_html = path.with_extension("_tmp.html");
        std::fs::write(&tmp_html, &html)?;
        let result = std::process::Command::new(&chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--no-pdf-header-footer",
                "--print-to-pdf-no-header",
                &format!("--print-to-pdf={}", path.display()),
                &format!("file://{}", tmp_html.display()),
            ])
            .output();
        let _ = std::fs::remove_file(&tmp_html);
        match result {
            Ok(output) if output.status.success() && path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) => {
                return Ok(json!({
                    "created": true,
                    "path": path.display().to_string(),
                    "format": "pdf",
                    "engine": "chrome",
                }));
            }
            Ok(output) => {
                tracing::warn!(
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "create_pdf: Chrome headless failed, falling back to genpdf"
                );
            }
            Err(e) => {
                tracing::warn!(%e, "create_pdf: Chrome not available, falling back to genpdf");
            }
        }
    }

    // Fallback: genpdf (no CJK support on most systems).
    let font = find_font_family().context("no usable fonts found for PDF generation")?;
    let mut doc = genpdf::Document::new(font);
    doc.set_title(title);

    if !title.is_empty() {
        let mut t = genpdf::elements::Paragraph::new(title);
        t.set_alignment(genpdf::Alignment::Center);
        doc.push(genpdf::elements::StyledElement::new(
            t,
            genpdf::style::Effect::Bold,
        ));
        doc.push(genpdf::elements::Break::new(1));
    }

    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if block.starts_with('#') {
            let text = block.trim_start_matches('#').trim();
            let mut p = genpdf::elements::Paragraph::new(text);
            p.set_alignment(genpdf::Alignment::Left);
            doc.push(genpdf::elements::StyledElement::new(
                p,
                genpdf::style::Effect::Bold,
            ));
        } else {
            doc.push(genpdf::elements::Paragraph::new(block));
        }
    }

    doc.render_to_file(path)
        .map_err(|e| anyhow!("pdf render failed: {e}"))?;
    Ok(json!({
        "created": true,
        "path": path.display().to_string(),
        "format": "pdf",
        "engine": "genpdf",
        "warning": "genpdf has limited CJK support. Install Chrome for better PDF rendering.",
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
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // [Content_Types].xml
    let mut ct_overrides = String::new();
    for i in 1..=slide_data.len() {
        ct_overrides.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
    }
    zip.start_file("[Content_Types].xml", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
{ct_overrides}
</Types>"#)?;

    // _rels/.rels
    zip.start_file("_rels/.rels", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#)?;

    // ppt/presentation.xml
    let mut slide_list = String::new();
    for i in 1..=slide_data.len() {
        slide_list.push_str(&format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 255 + i, i + 2));
    }
    zip.start_file("ppt/presentation.xml", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst>{slide_list}</p:sldIdLst>
<p:sldSz cx="12192000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#)?;

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
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{pres_rels}</Relationships>"#)?;

    // Minimal slide master
    zip.start_file("ppt/slideMasters/slideMaster1.xml", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#)?;

    zip.start_file("ppt/slideMasters/_rels/slideMaster1.xml.rels", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#)?;

    // Minimal slide layout
    zip.start_file("ppt/slideLayouts/slideLayout1.xml", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank">
<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>
</p:sldLayout>"#)?;

    zip.start_file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", opts)?;
    write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#)?;

    // Individual slides
    for (i, (title, body)) in slide_data.iter().enumerate() {
        let slide_num = i + 1;
        let title_esc = xml_escape(title);
        let body_esc = xml_escape(body);

        zip.start_file(format!("ppt/slides/slide{slide_num}.xml"), opts)?;
        write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
</p:sld>"#)?;

        zip.start_file(
            format!("ppt/slides/_rels/slide{slide_num}.xml.rels"),
            opts,
        )?;
        write!(zip, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#)?;
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
    use calamine::{Reader, open_workbook, Xlsx, Data};
    use rust_xlsxwriter::*;

    let mut wb_reader: Xlsx<_> = open_workbook(path)
        .map_err(|e| anyhow!("edit_excel: cannot open '{}': {e}", path.display()))?;

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

    // Apply "sheets" param: if a sheet name matches existing, replace it; otherwise add new.
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
            let parsed: Vec<Vec<Value>> = rows
                .iter()
                .filter_map(|r| r.as_array().cloned())
                .collect();
            appends.insert(sheet_name.to_owned(), parsed);
        }
    }
    // Also support append_rows as array of {sheet, rows} objects.
    if let Some(arr) = args["append_rows"].as_array() {
        for item in arr {
            let sheet_name = item["sheet"].as_str().unwrap_or("Sheet1");
            if let Some(rows) = item["rows"].as_array() {
                let parsed: Vec<Vec<Value>> = rows
                    .iter()
                    .filter_map(|r| r.as_array().cloned())
                    .collect();
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
                                    ws.write_number(
                                        row_idx,
                                        col_idx,
                                        n.as_f64().unwrap_or(0.0),
                                    )?;
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
// Edit Word (.docx) — read existing + append/replace content
// ---------------------------------------------------------------------------

fn edit_word(args: &Value, path: &Path) -> Result<Value> {
    use std::io::{Read as IoRead, Write};

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
        new_args["content"] = Value::String(replace_text.unwrap().to_owned());
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
    let append_text = append_text.unwrap();

    let file = std::fs::File::open(path)
        .map_err(|e| anyhow!("edit_word: cannot open '{}': {e}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| anyhow!("edit_word: invalid docx zip: {e}"))?;

    // Read all entries into memory.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
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
            new_paras.push_str(&format!(
                r#"<w:p><w:r><w:t>{escaped}</w:t></w:r></w:p>"#
            ));
        }
    }

    // Rewrite the zip with modified document.xml.
    let out_file = std::fs::File::create(path)?;
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
    writer.finish()?;

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
        .map_err(|e| anyhow!("edit_pdf: cannot open '{}': {e}", path.display()))?;

    let mut actions_done = Vec::new();

    // --- replace_text: find/replace in page content streams ---
    if let Some(replacements) = replacements {
        let mut total_replaced = 0usize;
        let page_ids: Vec<_> = doc.page_iter().collect();

        // Collect (stream_object_ids, modified_content) pairs first to avoid borrow conflicts.
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
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("doc");
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
        "pdf" => read_pdf(path),
        other => Err(anyhow!(
            "read_doc: unsupported format '.{other}'. Supported: xlsx, docx, pdf"
        )),
    }
}

fn read_excel(path: &Path) -> Result<Value> {
    use calamine::{Reader, open_workbook, Xlsx, Data};

    let mut wb: Xlsx<_> = open_workbook(path)
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

fn read_docx(path: &Path) -> Result<Value> {
    use std::io::Read as IoRead;

    let file = std::fs::File::open(path)
        .map_err(|e| anyhow!("read_doc: cannot open '{}': {e}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| anyhow!("read_doc: invalid docx zip: {e}"))?;

    let mut xml = String::new();
    if let Ok(mut entry) = archive.by_name("word/document.xml") {
        entry.read_to_string(&mut xml)?;
    } else {
        return Err(anyhow!("read_doc: word/document.xml not found in docx"));
    }

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
            while let Some(t_start) = inner.find("<w:t").map(|p| {
                // Skip to after the closing > of <w:t> or <w:t ...>
                inner[p..].find('>').map(|g| p + g + 1)
            }).flatten() {
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

/// Build a simple HTML document for Chrome headless PDF rendering.
fn build_html_for_pdf(title: &str, content: &str) -> String {
    let escaped_title = content.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let _ = escaped_title; // suppress warning, we use xml_escape below
    let mut html = String::from(
        r#"<!DOCTYPE html><html><head><meta charset='utf-8'>
<style>
  @page { margin: 2cm 2.5cm; size: A4; }
  @page { @top-left { content: none; } @top-right { content: none; } @bottom-left { content: none; } @bottom-right { content: none; } }
  body {
    font-family: -apple-system, "Microsoft YaHei", "PingFang SC", "Hiragino Sans GB", "Noto Sans CJK SC", system-ui, sans-serif;
    font-size: 13px; line-height: 1.9; color: #333;
  }
  h1 { text-align: center; font-size: 22px; font-weight: 600; margin: 0 0 24px; color: #1a1a1a; }
  h2 { font-size: 17px; font-weight: 600; margin: 20px 0 10px; color: #1a1a1a; border-bottom: 1px solid #e0e0e0; padding-bottom: 4px; }
  h3 { font-size: 15px; font-weight: 600; margin: 16px 0 8px; color: #333; }
  p  { margin: 8px 0; text-align: justify; }
  ul, ol { margin: 8px 0 8px 20px; }
  li { margin: 4px 0; }
  table { border-collapse: collapse; width: 100%; margin: 12px 0; }
  th { background: #f5f5f5; font-weight: 600; text-align: left; padding: 8px 12px; border: 1px solid #ddd; }
  td { padding: 8px 12px; border: 1px solid #ddd; }
  tr:nth-child(even) td { background: #fafafa; }
  code { background: #f4f4f4; padding: 2px 5px; border-radius: 3px; font-size: 12px; }
  pre { background: #f4f4f4; padding: 12px; border-radius: 4px; overflow-x: auto; font-size: 12px; }
  hr { border: none; border-top: 1px solid #e0e0e0; margin: 16px 0; }
</style></head><body>
"#,
    );
    if !title.is_empty() {
        html.push_str(&format!("<h1>{}</h1>\n", xml_escape(title)));
    }
    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        // Headings
        if block.starts_with("### ") {
            html.push_str(&format!("<h3>{}</h3>\n", xml_escape(&block[4..])));
        } else if block.starts_with("## ") {
            html.push_str(&format!("<h2>{}</h2>\n", xml_escape(&block[3..])));
        } else if block.starts_with("# ") {
            html.push_str(&format!("<h1>{}</h1>\n", xml_escape(&block[2..])));
        } else if block.starts_with("---") {
            html.push_str("<hr>\n");
        } else {
            // Check if block is a list (all lines start with - or * or 1.)
            let lines: Vec<&str> = block.lines().collect();
            let is_ul = lines.iter().all(|l| {
                let t = l.trim();
                t.starts_with("- ") || t.starts_with("* ")
            });
            let is_ol = lines.iter().all(|l| {
                let t = l.trim();
                t.len() > 2 && t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) && (t.contains(". ") || t.contains(") "))
            });
            if is_ul {
                html.push_str("<ul>\n");
                for line in &lines {
                    let text = line.trim().trim_start_matches("- ").trim_start_matches("* ");
                    html.push_str(&format!("<li>{}</li>\n", xml_escape(text)));
                }
                html.push_str("</ul>\n");
            } else if is_ol {
                html.push_str("<ol>\n");
                for line in &lines {
                    let text = line.trim();
                    // Strip "1. " or "1) " prefix
                    let text = if let Some(pos) = text.find(". ") {
                        &text[pos + 2..]
                    } else if let Some(pos) = text.find(") ") {
                        &text[pos + 2..]
                    } else {
                        text
                    };
                    html.push_str(&format!("<li>{}</li>\n", xml_escape(text)));
                }
                html.push_str("</ol>\n");
            } else {
                // Regular paragraph with line breaks
                html.push_str("<p>");
                html.push_str(&lines.iter().map(|l| xml_escape(l)).collect::<Vec<_>>().join("<br>"));
                html.push_str("</p>\n");
            }
        }
    }
    html.push_str("</body></html>");
    html
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
    use std::path::PathBuf;

    const TEST_CONTENT: &str = "今天晚上7点135号168栋会议室开视频会议，参会人员：张三13800138000、李四15912345678、王五18688886666";

    #[tokio::test]
    async fn test_create_word_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_doc_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.docx");

        let args = json!({
            "action": "create_word",
            "path": path.to_str().unwrap(),
            "content": TEST_CONTENT,
            "title": "Meeting Notice"
        });
        let result = handle(&args, &path).await.unwrap();
        assert_eq!(result["created"], true, "create_word failed: {result}");
        assert!(path.exists(), "docx file not created");

        // Read back
        let read_args = json!({"action": "read_doc", "path": path.to_str().unwrap()});
        let read_result = handle(&read_args, &path).await.unwrap();
        let text = read_result["text"].as_str().unwrap_or("");
        assert!(text.contains("135"), "docx missing '135': {text}");
        assert!(text.contains("168"), "docx missing '168': {text}");
        assert!(text.contains("13800138000"), "docx missing phone number: {text}");

        println!("DOCX output: {}", path.display());
    }

    #[tokio::test]
    async fn test_create_pdf_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_pdf_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.pdf");

        let args = json!({
            "action": "create_pdf",
            "path": path.to_str().unwrap(),
            "content": TEST_CONTENT,
            "title": "Meeting Notice"
        });
        let result = handle(&args, &path).await.unwrap();
        assert_eq!(result["created"], true, "create_pdf failed: {result}");
        assert!(path.exists(), "pdf file not created");
        assert!(path.metadata().unwrap().len() > 0, "pdf file is empty");

        // Read back via read_doc
        let read_args = json!({"action": "read_doc", "path": path.to_str().unwrap()});
        let read_result = handle(&read_args, &path).await.unwrap();
        let text = read_result["text"].as_str().unwrap_or("");
        println!("PDF read_doc text: '{text}'");
        // PDF text extraction may have spacing differences, just check numbers exist
        assert!(text.contains("135") || text.contains("1 3 5"), "pdf missing '135': {text}");
        assert!(text.contains("13800138000") || text.contains("1380013800"), "pdf missing phone: {text}");

        println!("PDF output: {}", path.display());
    }

    #[tokio::test]
    async fn test_create_excel_and_read_back() {
        let dir = std::env::temp_dir().join("rsclaw_xlsx_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.xlsx");

        let args = json!({
            "action": "create_excel",
            "path": path.to_str().unwrap(),
            "sheets": [{"name": "Sheet1", "headers": ["Name","Phone"], "rows": [["Zhang","13800138000"],["Li","15912345678"]]}]
        });
        let result = handle(&args, &path).await.unwrap();
        assert_eq!(result["created"], true, "create_excel failed: {result}");
        assert!(path.exists(), "xlsx file not created");

        // Read back — xlsx returns {"sheets": [{"rows": [...]}]}, not "text"
        let read_args = json!({"action": "read_doc", "path": path.to_str().unwrap()});
        let read_result = handle(&read_args, &path).await.unwrap();
        let sheets_json = serde_json::to_string(&read_result["sheets"]).unwrap_or_default();
        assert!(sheets_json.contains("13800138000"), "xlsx missing phone: {sheets_json}");

        // Don't clean up — let user inspect files
        println!("XLSX output: {}", path.display());
    }

    #[tokio::test]
    async fn test_create_word_empty_content_rejected() {
        let dir = std::env::temp_dir().join("rsclaw_empty_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.docx");

        let args = json!({"action": "create_word", "path": path.to_str().unwrap()});
        let result = handle(&args, &path).await.unwrap();
        assert!(result.get("error").is_some(), "empty content should return error: {result}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
