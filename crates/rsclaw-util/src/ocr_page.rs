//! Preparing a scanned page for the OCR model.
//!
//! The OCR model shrinks a whole A4 page before reading it, and small print
//! comes back wrong in a stable way (德→衡, 张→邬, a digit in a phone
//! number) while the same lines read right from a third of the page. A page
//! scanned sideways is worse: the model invents names and ID numbers and
//! often loops on one phrase. So a tall page of horizontal text is read in
//! strips cut along blank rows, and a sideways page is read both ways up.

use image::{DynamicImage, GenericImageView};

/// Long edge of every image sent to the OCR endpoint.
const OCR_MAX_EDGE: u32 = 2400;
const OCR_JPEG_QUALITY: u8 = 90;
/// Pages shorter than this read fine whole.
const MIN_STRIP_PAGE_HEIGHT: u32 = 1600;
/// Strips about twice as wide as tall read as well as a cropped header.
const STRIP_ASPECT: f64 = 0.5;
const MAX_STRIPS: usize = 4;
/// Share of a strip's pixels that must be ink for it to be read.
const MIN_STRIP_INK: f64 = 0.002;
/// A pixel is ink when its brightest channel is below this: black and dark
/// grey text count, a red seal does not.
const INK_MAX_CHANNEL: u8 = 110;
/// Edge of the thumbnail the text direction is judged on.
const AXIS_PROBE_EDGE: u32 = 1000;
/// Looser than `INK_MAX_CHANNEL` so grey print and photos count when
/// finding the printed part of a page.
const CONTENT_MAX_CHANNEL: u8 = 170;
/// Crop to the printed part only when it covers less of the page than this.
const CROP_MAX_AREA: f64 = 0.6;

/// Which way the lines of text on a page run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAxis {
    Horizontal,
    /// The page was scanned or photographed on its side.
    Vertical,
    Unknown,
}

/// How to read one page.
#[derive(Debug)]
pub enum OcrPlan {
    /// Send the image as it is.
    Whole,
    /// Read these JPEG strips top to bottom and join their text. A single
    /// strip is the printed part of a mostly blank page.
    Strips(Vec<Vec<u8>>),
    /// The text runs sideways: the page turned 90° and 270° clockwise.
    /// Read both; the right way up reads the same whole and in part.
    Rotations([Turned; 2]),
}

/// A sideways page turned one way: all of it, and its top part.
#[derive(Debug)]
pub struct Turned {
    pub whole: Vec<u8>,
    pub top: Vec<u8>,
}

/// Decides how to read `data`. Anything that does not decode as an image
/// is read whole.
pub fn plan_ocr_page(data: &[u8]) -> OcrPlan {
    let Ok(page) = image::load_from_memory(data) else {
        return OcrPlan::Whole;
    };
    let cropped = crop_to_content(&page);
    let img = cropped.as_ref().unwrap_or(&page);
    let plan = match text_axis(img) {
        TextAxis::Vertical => rotations(img).map(OcrPlan::Rotations),
        TextAxis::Horizontal => strips(img).map(OcrPlan::Strips),
        TextAxis::Unknown => None,
    };
    plan.or_else(|| Some(OcrPlan::Strips(vec![encode(cropped.as_ref()?)?])))
        .unwrap_or(OcrPlan::Whole)
}

/// The printed part of the page plus a margin, when it is clearly smaller
/// than the page: an ID card or a receipt scanned on an A4 sheet reads
/// better at the card's size.
fn crop_to_content(img: &DynamicImage) -> Option<DynamicImage> {
    let probe = img.thumbnail(AXIS_PROBE_EDGE, AXIS_PROBE_EDGE).to_rgb8();
    let (pw, ph) = probe.dimensions();
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (x, y, p) in probe.enumerate_pixels() {
        if p.0.iter().max().copied().unwrap_or(255) < CONTENT_MAX_CHANNEL {
            xs.push(x);
            ys.push(y);
        }
    }
    if xs.len() < 200 {
        return None;
    }
    let (x0, x1) = percentile_span(&mut xs, 0.005);
    let (y0, y1) = percentile_span(&mut ys, 0.005);
    let margin = pw.max(ph) / 50;
    let (x0, y0) = (x0.saturating_sub(margin), y0.saturating_sub(margin));
    let (x1, y1) = ((x1 + margin).min(pw - 1), (y1 + margin).min(ph - 1));
    let area = f64::from(x1 - x0 + 1) * f64::from(y1 - y0 + 1);
    if area >= f64::from(pw) * f64::from(ph) * CROP_MAX_AREA {
        return None;
    }
    let (w, h) = img.dimensions();
    let (sx, sy) = (f64::from(w) / f64::from(pw), f64::from(h) / f64::from(ph));
    let left = (f64::from(x0) * sx) as u32;
    let top = (f64::from(y0) * sy) as u32;
    let right = ((f64::from(x1 + 1) * sx).ceil() as u32).min(w);
    let bottom = ((f64::from(y1 + 1) * sy).ceil() as u32).min(h);
    Some(img.crop_imm(left, top, right - left, bottom - top))
}

/// Judges the text direction from the share of blank rows and blank
/// columns inside the inked area: lines of text leave blank rows between
/// them and almost no blank columns.
pub fn text_axis(img: &DynamicImage) -> TextAxis {
    let probe = img.thumbnail(AXIS_PROBE_EDGE, AXIS_PROBE_EDGE).to_rgb8();
    let w = probe.width();
    let ink: Vec<bool> = probe
        .pixels()
        .map(|p| p.0.iter().max().copied().unwrap_or(255) < INK_MAX_CHANNEL)
        .collect();
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (i, &on) in ink.iter().enumerate() {
        if on {
            xs.push(i as u32 % w);
            ys.push(i as u32 / w);
        }
    }
    if xs.len() < 200 {
        return TextAxis::Unknown;
    }
    // Trim stray specks at the edges of the scan.
    let (x0, x1) = percentile_span(&mut xs, 0.02);
    let (y0, y1) = percentile_span(&mut ys, 0.02);
    if x1 <= x0 || y1 <= y0 {
        return TextAxis::Unknown;
    }
    let mut rows = vec![0u32; (y1 - y0 + 1) as usize];
    let mut cols = vec![0u32; (x1 - x0 + 1) as usize];
    for y in y0..=y1 {
        for x in x0..=x1 {
            if ink[(y * w + x) as usize] {
                rows[(y - y0) as usize] += 1;
                cols[(x - x0) as usize] += 1;
            }
        }
    }
    let (row_blank, col_blank) = (blank_share(&rows), blank_share(&cols));
    if row_blank >= 0.25 && row_blank - col_blank >= 0.15 {
        TextAxis::Horizontal
    } else if col_blank >= 0.25 && col_blank - row_blank >= 0.15 {
        TextAxis::Vertical
    } else {
        TextAxis::Unknown
    }
}

/// The `q` and `1 - q` quantiles of `v`.
fn percentile_span(v: &mut [u32], q: f64) -> (u32, u32) {
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    (at(q), at(1.0 - q))
}

/// Share of lines in an ink profile that are nearly empty.
fn blank_share(profile: &[u32]) -> f64 {
    let peak = profile.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return 0.0;
    }
    let blank = profile.iter().filter(|&&n| f64::from(n) <= f64::from(peak) * 0.05).count();
    blank as f64 / profile.len() as f64
}

/// Cuts a tall page into strips at the blankest row near each even cut.
/// `None` when the page is short or wide enough to read whole.
fn strips(img: &DynamicImage) -> Option<Vec<Vec<u8>>> {
    let (w, h) = img.dimensions();
    if h < MIN_STRIP_PAGE_HEIGHT || h < w {
        return None;
    }
    let n = ((f64::from(h) / (f64::from(w) * STRIP_ASPECT)).round() as usize).clamp(2, MAX_STRIPS);
    let rgb = img.to_rgb8();
    let row_ink: Vec<u32> = rgb
        .rows()
        .map(|row| row.filter(|p| p.0.iter().max().copied().unwrap_or(255) < INK_MAX_CHANNEL).count() as u32)
        .collect();
    let window = h as usize / (n * 4);
    let mut cuts = vec![0usize];
    for k in 1..n {
        let even = h as usize * k / n;
        let lo = even.saturating_sub(window).max(cuts[k - 1] + 1);
        let hi = (even + window).min(h as usize - 1);
        // Blankest row; the one nearest the even cut on a tie.
        let cut = (lo..=hi)
            .min_by_key(|&y| (row_ink[y], y.abs_diff(even)))
            .unwrap_or(even);
        cuts.push(cut);
    }
    cuts.push(h as usize);
    // A strip with next to no ink (the blank lower part of a short page) is
    // left out: some OCR models answer a blank image with paragraphs of
    // invented text.
    let parts: Vec<Vec<u8>> = cuts
        .windows(2)
        .filter(|c| {
            let ink: u64 = row_ink[c[0]..c[1]].iter().map(|&n| u64::from(n)).sum();
            ink as f64 >= (c[1] - c[0]) as f64 * f64::from(w) * MIN_STRIP_INK
        })
        .map(|c| encode(&img.crop_imm(0, c[0] as u32, w, (c[1] - c[0]) as u32)))
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// Share of a turned page kept as its top part.
const TOP_PART: f64 = 0.55;

fn rotations(img: &DynamicImage) -> Option<[Turned; 2]> {
    let turn = |img: DynamicImage| {
        let (w, h) = img.dimensions();
        let top = img.crop_imm(0, 0, w, (f64::from(h) * TOP_PART) as u32);
        Some(Turned { whole: encode(&img)?, top: encode(&top)? })
    };
    Some([turn(img.rotate90())?, turn(img.rotate270())?])
}

/// JPEG within the endpoint's size limits.
fn encode(img: &DynamicImage) -> Option<Vec<u8>> {
    let (w, h) = img.dimensions();
    let img = if w.max(h) > OCR_MAX_EDGE {
        img.resize(OCR_MAX_EDGE, OCR_MAX_EDGE, image::imageops::FilterType::Lanczos3)
    } else {
        img.clone()
    };
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, OCR_JPEG_QUALITY);
    img.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// A white page with dark text lines: horizontal bars `line` px tall
    /// every `pitch` px, or vertical ones when `sideways`.
    fn page(w: u32, h: u32, line: u32, pitch: u32, sideways: bool) -> DynamicImage {
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        for (x, y, p) in img.enumerate_pixels_mut() {
            let (along, across, len) = if sideways { (y, x, h) } else { (x, y, w) };
            let in_margin = along < len / 10 || along > len * 9 / 10 || across < 100;
            if !in_margin && across % pitch < line {
                *p = Rgb([20, 20, 20]);
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn ocr_page_tells_horizontal_from_sideways_text() {
        assert_eq!(text_axis(&page(1240, 1754, 30, 70, false)), TextAxis::Horizontal);
        assert_eq!(text_axis(&page(1240, 1754, 30, 70, true)), TextAxis::Vertical);
        let blank = DynamicImage::ImageRgb8(RgbImage::from_pixel(800, 1000, Rgb([255, 255, 255])));
        assert_eq!(text_axis(&blank), TextAxis::Unknown);
    }

    #[test]
    fn ocr_page_cuts_a_tall_page_between_lines() {
        // An A4 page at about 240 dpi, under the size limit so strips keep
        // their pixel rows.
        let img = page(2000, 2828, 40, 100, false);
        let strips = strips(&img).expect("a tall A4 page is cut");
        assert_eq!(strips.len(), 3);
        let mut top = 0;
        for jpeg in &strips {
            let strip = image::load_from_memory(jpeg).unwrap();
            // Each cut falls in a gap: the strip's first row is blank
            // unless it is the top of the page.
            if top > 0 {
                assert!(top % 100 >= 40, "cut at row {top} splits a line");
            }
            top += strip.height();
        }
        assert_eq!(top, 2828);
    }

    #[test]
    fn ocr_page_leaves_out_blank_strips() {
        // Text only in the top third of a tall page.
        let mut img = RgbImage::from_pixel(2000, 2828, Rgb([255, 255, 255]));
        for (x, y, p) in img.enumerate_pixels_mut() {
            if (200..1800).contains(&x) && (100..900).contains(&y) && y % 100 < 40 {
                *p = Rgb([20, 20, 20]);
            }
        }
        let parts = strips(&DynamicImage::ImageRgb8(img)).expect("the inked strip is read");
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn ocr_page_reads_small_or_wide_pages_whole() {
        assert!(strips(&page(1000, 1400, 30, 70, false)).is_none());
        assert!(strips(&page(3000, 2000, 30, 70, false)).is_none());
    }

    #[test]
    fn ocr_page_crops_a_card_on_a_blank_sheet() {
        let mut sheet = RgbImage::from_pixel(1654, 2339, Rgb([255, 255, 255]));
        for (x, y, p) in sheet.enumerate_pixels_mut() {
            if (600..1100).contains(&x) && (900..1200).contains(&y) && y % 40 < 15 {
                *p = Rgb([20, 20, 20]);
            }
        }
        let card = crop_to_content(&DynamicImage::ImageRgb8(sheet)).expect("the card is cropped");
        let (w, h) = card.dimensions();
        assert!(w < 700 && h < 500, "crop {w}x{h}");
        assert!(crop_to_content(&page(1240, 1754, 30, 70, false)).is_none());
    }

    #[test]
    fn ocr_page_turns_a_sideways_page_both_ways() {
        let mut jpeg = Vec::new();
        page(1240, 1754, 30, 70, true)
            .to_rgb8()
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90))
            .unwrap();
        let OcrPlan::Rotations([a, b]) = plan_ocr_page(&jpeg) else {
            panic!("a sideways page is read both ways up");
        };
        for r in [a, b] {
            let img = image::load_from_memory(&r.whole).unwrap();
            assert_eq!(img.dimensions(), (1754, 1240));
            let top = image::load_from_memory(&r.top).unwrap();
            assert_eq!(top.dimensions(), (1754, 682));
        }
    }
}
