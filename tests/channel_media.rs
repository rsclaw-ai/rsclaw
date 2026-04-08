//! Integration tests for media type detection and Office document text extraction.

use rsclaw::channel::{
    extract_office_text, is_audio_attachment, is_image_attachment, is_video_attachment,
};

// ---------------------------------------------------------------------------
// Image detection
// ---------------------------------------------------------------------------

#[test]
fn image_detection_by_mime() {
    let image_mimes = [
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "image/bmp",
        "image/svg+xml",
        "image/tiff",
        "image/x-icon",
        "image/heic",
        "image/avif",
    ];
    for mime in image_mimes {
        assert!(
            is_image_attachment(mime, ""),
            "MIME '{mime}' should be detected as image"
        );
    }
    // Non-image MIME types.
    assert!(!is_image_attachment("video/mp4", ""), "video/mp4 is not image");
    assert!(
        !is_image_attachment("application/pdf", ""),
        "application/pdf is not image"
    );
    assert!(
        !is_image_attachment("audio/mpeg", ""),
        "audio/mpeg is not image"
    );
}

#[test]
fn image_detection_by_extension() {
    let extensions = [
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "tiff", "ico", "heic", "heif", "avif",
    ];
    for ext in extensions {
        // Lowercase.
        let filename = format!("photo.{ext}");
        assert!(
            is_image_attachment("", &filename),
            "extension '.{ext}' should be detected as image"
        );
        // Uppercase.
        let filename_upper = format!("photo.{}", ext.to_uppercase());
        assert!(
            is_image_attachment("", &filename_upper),
            "extension '.{}' (uppercase) should be detected as image",
            ext.to_uppercase()
        );
    }
    // Non-image extensions.
    assert!(!is_image_attachment("", "video.mp4"));
    assert!(!is_image_attachment("", "doc.pdf"));
}

// ---------------------------------------------------------------------------
// Audio detection
// ---------------------------------------------------------------------------

#[test]
fn audio_detection_by_mime() {
    let audio_mimes = [
        "audio/mpeg",
        "audio/ogg",
        "audio/wav",
        "audio/flac",
        "audio/aac",
        "audio/amr",
    ];
    for mime in audio_mimes {
        assert!(
            is_audio_attachment(mime, ""),
            "MIME '{mime}' should be detected as audio"
        );
    }
    // Special "voice" type.
    assert!(
        is_audio_attachment("voice", ""),
        "'voice' content type should be detected as audio"
    );
    // Non-audio.
    assert!(!is_audio_attachment("video/mp4", ""));
    assert!(!is_audio_attachment("image/png", ""));
}

#[test]
fn audio_detection_by_extension() {
    let extensions = [
        "amr", "ogg", "opus", "silk", "wav", "mp3", "m4a", "aac", "flac", "wma",
    ];
    for ext in extensions {
        let filename = format!("recording.{ext}");
        assert!(
            is_audio_attachment("", &filename),
            "extension '.{ext}' should be detected as audio"
        );
    }
    // Non-audio extension.
    assert!(!is_audio_attachment("", "image.png"));
}

// ---------------------------------------------------------------------------
// Video detection
// ---------------------------------------------------------------------------

#[test]
fn video_detection_by_mime() {
    let video_mimes = ["video/mp4", "video/webm", "video/quicktime", "video/x-msvideo"];
    for mime in video_mimes {
        assert!(
            is_video_attachment(mime, ""),
            "MIME '{mime}' should be detected as video"
        );
    }
    // Non-video.
    assert!(!is_video_attachment("audio/mpeg", ""));
    assert!(!is_video_attachment("image/png", ""));
}

#[test]
fn video_detection_by_extension() {
    let extensions = ["mp4", "mov", "avi", "mkv", "webm", "wmv", "flv", "3gp"];
    for ext in extensions {
        let filename = format!("clip.{ext}");
        assert!(
            is_video_attachment("", &filename),
            "extension '.{ext}' should be detected as video"
        );
    }
    // Non-video extension.
    assert!(!is_video_attachment("", "song.mp3"));
}

// ---------------------------------------------------------------------------
// Office document extraction helpers
// ---------------------------------------------------------------------------

fn create_minimal_docx(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        use std::io::Write;
        write!(
            zip,
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn create_minimal_xlsx(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file(
            "xl/sharedStrings.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        write!(
            zip,
            r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>{text}</t></si></sst>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn create_minimal_pptx(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file(
            "ppt/slides/slide1.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        write!(
            zip,
            r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }
    buf
}

// ---------------------------------------------------------------------------
// Office extraction tests
// ---------------------------------------------------------------------------

#[test]
fn extract_docx_text() {
    let bytes = create_minimal_docx("Hello from Word");
    let result = extract_office_text("report.docx", &bytes);
    assert!(result.is_some(), "should extract text from docx");
    let text = result.unwrap();
    assert!(
        text.contains("Hello from Word"),
        "extracted text should contain input: {text}"
    );
}

#[test]
fn extract_xlsx_text() {
    let bytes = create_minimal_xlsx("Spreadsheet data");
    let result = extract_office_text("data.xlsx", &bytes);
    assert!(result.is_some(), "should extract text from xlsx");
    let text = result.unwrap();
    assert!(
        text.contains("Spreadsheet data"),
        "extracted text should contain input: {text}"
    );
}

#[test]
fn extract_pptx_text() {
    let bytes = create_minimal_pptx("Slide content here");
    let result = extract_office_text("slides.pptx", &bytes);
    assert!(result.is_some(), "should extract text from pptx");
    let text = result.unwrap();
    assert!(
        text.contains("Slide content here"),
        "extracted text should contain input: {text}"
    );
}

#[test]
fn extract_unsupported_returns_none() {
    let result = extract_office_text("image.png", &[0x89, 0x50, 0x4e, 0x47]);
    assert!(
        result.is_none(),
        "unsupported file type should return None"
    );
}

#[test]
fn extract_corrupt_zip_returns_none() {
    let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33];
    let result = extract_office_text("broken.docx", &garbage);
    assert!(
        result.is_none(),
        "corrupt ZIP data should return None, not panic"
    );
}
