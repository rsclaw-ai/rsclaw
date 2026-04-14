//! Built-in `doc` tool — create Office documents natively.
//!
//! Supported actions:
//!   - create_excel (.xlsx) via rust_xlsxwriter
//!   - create_word  (.docx) via docx-rs
//!   - create_pdf   (.pdf)  via genpdf
//!   - create_ppt   (.pptx) via zip + OOXML templates

use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn handle(args: &Value, full_path: &Path) -> Result<Value> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow!("doc: `action` required"))?
        .to_owned();

    // All crate operations are synchronous — run on blocking pool.
    let args = args.clone();
    let path = full_path.to_path_buf();

    let result = tokio::task::spawn_blocking(move || match action.as_str() {
        "create_excel" => create_excel(&args, &path),
        "create_word" => create_word(&args, &path),
        "create_pdf" => create_pdf(&args, &path),
        "create_ppt" => create_ppt(&args, &path),
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

    // Header format: bold.
    let header_fmt = Format::new().set_bold();

    let sheets = args["sheets"].as_array();
    if let Some(sheets) = sheets {
        for sheet_def in sheets {
            let name = sheet_def["name"].as_str().unwrap_or("Sheet");
            let ws = workbook.add_worksheet();
            ws.set_name(name)?;

            // Write headers.
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

            // Write data rows.
            if let Some(rows) = sheet_def["rows"].as_array() {
                for (r, row) in rows.iter().enumerate() {
                    if let Some(cells) = row.as_array() {
                        for (c, cell) in cells.iter().enumerate() {
                            let row_idx = (r + 1) as u32; // +1 for header
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
    } else {
        // No sheets provided — create empty sheet.
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

    let mut docx = Docx::new();

    // Title paragraph.
    if !title.is_empty() {
        let p = Paragraph::new()
            .add_run(Run::new().add_text(title).bold())
            .style("Heading1");
        docx = docx.add_paragraph(p);
        docx = docx.add_paragraph(Paragraph::new()); // blank line
    }

    // Content: split by double newlines into paragraphs.
    // Lines starting with # become headings.
    for block in content.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if block.starts_with("### ") {
            let text = &block[4..];
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(Run::new().add_text(text).bold())
                    .style("Heading3"),
            );
        } else if block.starts_with("## ") {
            let text = &block[3..];
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(Run::new().add_text(text).bold())
                    .style("Heading2"),
            );
        } else if block.starts_with("# ") {
            let text = &block[2..];
            docx = docx.add_paragraph(
                Paragraph::new()
                    .add_run(Run::new().add_text(text).bold())
                    .style("Heading1"),
            );
        } else {
            // Regular paragraph — handle line breaks within.
            let p = Paragraph::new().add_run(Run::new().add_text(block));
            docx = docx.add_paragraph(p);
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

    // Use genpdf with built-in Courier font (always available, no file needed).
    let font =
        genpdf::fonts::from_files("", "Courier", None).unwrap_or_else(|_| {
            // Fallback: use the default font family.
            genpdf::fonts::from_files("/usr/share/fonts/truetype/liberation", "LiberationSans", None)
                .unwrap_or_else(|_| {
                    // macOS fallback
                    genpdf::fonts::from_files("/System/Library/Fonts", "Helvetica", None)
                        .unwrap_or_else(|_| {
                            // Last resort: Courier from macOS
                            genpdf::fonts::from_files("/System/Library/Fonts", "Courier", None)
                                .expect("no fonts available")
                        })
                })
        });

    let mut doc = genpdf::Document::new(font);
    doc.set_title(title);

    // Title.
    if !title.is_empty() {
        let mut t = genpdf::elements::Paragraph::new(title);
        t.set_alignment(genpdf::Alignment::Center);
        doc.push(genpdf::elements::StyledElement::new(
            t,
            genpdf::style::Effect::Bold,
        ));
        doc.push(genpdf::elements::Break::new(1));
    }

    // Content paragraphs.
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

/// Escape XML special characters.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
