//! PDF pages to images, in pure Rust (hayro): no poppler / pdftoppm, so it
//! works the same on Windows, macOS and Linux.
//!
//! Pages are sized for OCR: at most 300 dpi and at most `max_edge` pixels on
//! the long side. A scan placed on an oversized page (a 6144x8192 phone
//! photo on a 4608x6144 pt page) would otherwise render at 300 dpi to a
//! 19200x25600 image nothing can read.

use anyhow::{Result, anyhow};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

/// Long edge, in pixels, of a page rendered for OCR.
pub const OCR_MAX_EDGE: u32 = 3000;
/// Highest resolution a page is rendered at.
const MAX_DPI: f32 = 300.0;

/// Number of pages in the PDF.
pub fn page_count(bytes: &[u8]) -> Result<usize> {
    let data = bytes.to_vec();
    std::panic::catch_unwind(move || {
        Pdf::new(data)
            .map(|pdf| pdf.pages().len())
            .map_err(|e| anyhow!("pdf: cannot open: {e:?}"))
    })
    .map_err(|_| anyhow!("pdf: the PDF parser crashed on this file"))?
}

/// Renders the given 0-based pages as JPEGs no larger than `max_edge` on
/// their long side. One result per requested page, in order; a page that
/// fails gets its error without stopping the rest.
pub fn render_pages_jpeg(bytes: &[u8], pages: &[usize], max_edge: u32) -> Result<Vec<Result<Vec<u8>>>> {
    let data = bytes.to_vec();
    let pages = pages.to_vec();
    std::panic::catch_unwind(move || -> Result<Vec<Result<Vec<u8>>>> {
        let pdf = Pdf::new(data).map_err(|e| anyhow!("pdf: cannot open: {e:?}"))?;
        let all = pdf.pages();
        let cache = RenderCache::new();
        let interpreter = InterpreterSettings::default();
        Ok(pages
            .iter()
            .map(|&index| {
                let page = all.get(index).ok_or_else(|| anyhow!("pdf: no page {}", index + 1))?;
                let (w, h) = page.render_dimensions();
                let scale = page_scale(w, h, max_edge);
                let pixmap = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hayro::render(
                        page,
                        &cache,
                        &interpreter,
                        &RenderSettings::default(),
                        &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
                    )
                }))
                .map_err(|_| anyhow!("pdf: page {} could not be rendered", index + 1))?;
                encode_jpeg(pixmap.width(), pixmap.height(), pixmap.data_as_u8_slice())
            })
            .collect())
    })
    .map_err(|_| anyhow!("pdf: the PDF renderer crashed on this file"))?
}

/// Scale from PDF points to pixels: 300 dpi, shrunk so the long edge fits
/// `max_edge`.
fn page_scale(width_pt: f32, height_pt: f32, max_edge: u32) -> f32 {
    let dpi_scale = MAX_DPI / 72.0;
    let long = width_pt.max(height_pt).max(1.0);
    dpi_scale.min(max_edge as f32 / long)
}

/// Opaque RGBA (rendered on white) to a JPEG.
fn encode_jpeg(width: u16, height: u16, rgba: &[u8]) -> Result<Vec<u8>> {
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let img = image::RgbImage::from_raw(u32::from(width), u32::from(height), rgb)
        .ok_or_else(|| anyhow!("pdf: rendered page has the wrong size"))?;
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .map_err(|e| anyhow!("pdf: cannot encode page: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_scale_caps_resolution_and_size() {
        // A4 (595x842 pt): 300 dpi would be 2480x3508; capped to 3000.
        let s = page_scale(595.0, 842.0, 3000);
        assert!((842.0 * s - 3000.0).abs() < 1.0, "{s}");
        // A small page stays at 300 dpi rather than being blown up.
        assert!((page_scale(200.0, 300.0, 3000) - 300.0 / 72.0).abs() < 1e-4);
        // The 4608x6144 pt page from a real case file fits too.
        assert!((6144.0 * page_scale(4608.0, 6144.0, 3000) - 3000.0).abs() < 1.0);
    }

    #[test]
    fn renders_a_pdf_page_to_jpeg() {
        // A one-page PDF built with krilla (already a dependency).
        let pdf = {
            use krilla::Document;
            use krilla::page::PageSettings;
            let mut doc = Document::new();
            let page = doc.start_page_with(PageSettings::from_wh(200.0, 300.0).unwrap());
            page.finish();
            doc.finish().unwrap()
        };
        assert_eq!(page_count(&pdf).unwrap(), 1);
        let pages = render_pages_jpeg(&pdf, &[0, 5], 3000).unwrap();
        let jpeg = pages[0].as_ref().unwrap();
        let img = image::load_from_memory(jpeg).unwrap();
        // 200x300 pt at 300 dpi.
        assert_eq!((img.width(), img.height()), (833, 1250));
        assert!(pages[1].is_err(), "page 6 doesn't exist");
    }
}
