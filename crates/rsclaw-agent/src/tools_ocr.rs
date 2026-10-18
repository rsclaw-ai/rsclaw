//! OCR agent tool. The `OcrClient` itself now lives in the `rsclaw-kb`
//! crate (the KB image/scanned-PDF canonicalizer is its primary user);
//! re-exported here so existing `crate::tools_ocr::OcrClient`
//! call sites keep resolving. This file keeps the `ocr` agent tool
//! handler, which is bound to `AgentRuntime` and therefore stays in root.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Result, anyhow};
pub use rsclaw_kb::OcrClient;
use futures::StreamExt as _;
use serde_json::{Value, json};

/// Server-side failures in a row, and the unix second until which the OCR
/// endpoint is treated as down. A model reading a scanned bundle otherwise
/// tries every page against a dead endpoint (50 calls in one case).
static OCR_SERVER_FAILURES: AtomicU32 = AtomicU32::new(0);
static OCR_DOWN_UNTIL: AtomicU64 = AtomicU64::new(0);
const OCR_FAILURES_TO_TRIP: u32 = 3;
const OCR_DOWN_SECS: u64 = 120;
/// Back-off before each retry of a busy or failing endpoint.
const OCR_RETRY_DELAYS_MS: [u64; 3] = [500, 1_000, 2_000];

/// OCR requests in flight at once. Each fleet OCR node takes one image at a
/// time and the gateway spreads requests over ten of them by load, so stay
/// a little under the node count; a rare collision still gets a 502
/// `backend_overload`, which the retries below absorb.
fn ocr_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(8))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether an OCR error is the endpoint's fault (down, overloaded,
/// unreachable) rather than the image's.
fn is_server_failure(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("server error")
        || e.contains("error sending request")
        || e.contains("timed out")
        || e.contains("busy")
}

impl super::runtime::AgentRuntime {
    /// OCR an image to verbatim text via the `kb.ocr` endpoint. Accepts a
    /// workspace/uploads file path, an http(s) URL, or a data URI.
    pub(crate) async fn tool_ocr(
        &self,
        ctx: &super::runtime::RunContext,
        args: Value,
    ) -> Result<Value> {
        let image = args["image"].as_str().unwrap_or("").trim();
        if image.is_empty() {
            return Ok(json!({
                "error": "ocr: `image` is required",
                "hint": "Pass a file path, an http(s):// URL, or a data:image/...;base64,... URI."
            }));
        }
        if now_secs() < OCR_DOWN_UNTIL.load(Ordering::Relaxed) {
            return Ok(json!({
                "error": "ocr: the OCR service is unavailable right now (several requests in a row failed on the server)",
                "hint": "Do not call ocr again in this turn. Work with the text you already have, and tell the user which scanned pages could not be read."
            }));
        }
        let Some(client) = OcrClient::from_config() else {
            return Ok(json!({
                "error": "ocr: no OCR endpoint configured",
                "hint": format!(
                    "Set `kb.ocr` in {} (model rsclaw-ocr-v1 needs no baseUrl), then retry.",
                    rsclaw_brand::CONFIG_FILE
                )
            }));
        };

        let lang = args["lang"].as_str().map(str::to_owned);
        let prompt = args["prompt"].as_str().map(str::to_owned);
        let max_tokens = match parse_max_tokens(&args) {
            Ok(max_tokens) => max_tokens,
            Err(message) => return Ok(json!({ "error": message })),
        };

        // Resolve the image to something the endpoint accepts: URLs and
        // data URIs pass through; a local path is read and base64-wrapped.
        let reading = if image.starts_with("http://")
            || image.starts_with("https://")
            || image.starts_with("data:")
        {
            Reading::Whole(image.to_owned())
        } else {
            // Scope-checked: the bytes are uploaded to the OCR endpoint, so
            // non-owners must not be able to point this at host files.
            let path = match self.resolve_readable_path(ctx, image) {
                Ok(p) => p,
                Err(e) => return Ok(json!({ "error": format!("ocr: {e}") })),
            };
            // Size first, from the header alone: an odd PDF page rendered
            // at 300 dpi came out 19200x25600 px (48 MB), which can't be
            // decoded or sent, and the model's own attempts to shrink it
            // with a guessed tool flag failed the turn.
            if let Some((w, h)) = image_dimensions(&path) {
                if u64::from(w) * u64::from(h) > MAX_OCR_PIXELS {
                    return Ok(json!({
                        "error": format!("ocr: `{image}` is {w}x{h} pixels, too large to read"),
                        "hint": SHRINK_HINT,
                    }));
                }
            }
            let bytes = match super::security::read_regular_file_capped(
                &path,
                super::security::MAX_LOCAL_READ_BYTES,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    return Ok(json!({
                        "error": format!("ocr: cannot read image file `{image}`: {e}"),
                        "hint": format!("Use a path relative to the workspace, or pass an http(s) URL / data URI. If the file is too large: {SHRINK_HINT}")
                    }));
                }
            };
            let mime = rsclaw_kb::canonicalize::detect_mime(&bytes, Some(image));
            // A custom prompt may ask for coordinates, which only mean
            // something on the image as given.
            let plan_page = prompt.is_none();
            tokio::task::spawn_blocking(move || plan_reading(bytes, mime, plan_page))
                .await
                .map_err(|e| anyhow!("ocr: task join failed: {e}"))?
        };

        let request = OcrRequest { client, prompt, max_tokens, lang };
        let (text, note) = match reading {
            Reading::Whole(payload) => {
                let read = request.read(payload).await.map(|r| r.text);
                let note = read.as_ref().ok().filter(|t| looping_text(t)).map(|_| {
                    "The reading repeats itself; parts of it are likely invented. Don't rely on it."
                        .to_owned()
                });
                (read, note)
            }
            Reading::Strips(parts) => {
                let count = parts.len();
                // A strip is a third of a page: cap its reading so a model
                // that loops can't run on for pages.
                let strip_request = OcrRequest {
                    max_tokens: request.max_tokens.or(Some(STRIP_MAX_TOKENS)),
                    ..request.clone()
                };
                let reads: Vec<_> =
                    futures::stream::iter(parts.into_iter().map(|p| strip_request.read(p)))
                        .buffered(STRIPS_IN_FLIGHT)
                        .collect()
                        .await;
                match reads.into_iter().collect::<Result<Vec<_>, _>>() {
                    Ok(reads) => {
                        let looping = reads.iter().filter(|r| looping_text(&r.text)).count();
                        let text = reads
                            .into_iter()
                            .filter(|r| !looping_text(&r.text))
                            .map(|r| r.text)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut note = (count > 1).then(|| format!("Read in {count} strips, top to bottom."));
                        if looping > 0 {
                            note = Some(format!(
                                "{}{looping} strip(s) came back as a repeating loop and were left out; that part of the page may be unread.",
                                note.map(|n| format!("{n} ")).unwrap_or_default()
                            ));
                        }
                        (Ok(text), note)
                    }
                    Err(e) => (Err(e), None),
                }
            }
            Reading::Rotations([turned_90, turned_270]) => {
                let payloads = [turned_90.0, turned_90.1, turned_270.0, turned_270.1];
                let reads: Vec<_> = futures::stream::iter(payloads.into_iter().map(|p| request.read(p)))
                    .buffered(STRIPS_IN_FLIGHT)
                    .collect()
                    .await;
                let [whole_90, top_90, whole_270, top_270]: [_; 4] =
                    reads.try_into().unwrap_or_else(|_| unreachable!("four reads"));
                let (text, degrees) = match (whole_90, whole_270) {
                    (Ok(a), Ok(b)) => match (top_90, top_270) {
                        (Ok(ta), Ok(tb)) => pick_consistent(a, ta, b, tb),
                        _ => pick_upright(a, b),
                    },
                    (Ok(a), Err(_)) => (a.text, 90),
                    (Err(_), Ok(b)) => (b.text, 270),
                    (Err(e), Err(_)) => return Ok(failure(e)),
                };
                let note = format!("The page was sideways; read after turning it {degrees}° clockwise.");
                (Ok(text), Some(note))
            }
        };

        match text {
            Ok(t) => {
                OCR_SERVER_FAILURES.store(0, Ordering::Relaxed);
                let mut out = if t.trim().is_empty() {
                    json!({ "text": "", "note": "OCR returned no text (blank or non-text image)." })
                } else {
                    json!({ "text": t.trim() })
                };
                if let (Some(note), false) = (note, t.trim().is_empty()) {
                    out["note"] = json!(note);
                }
                Ok(out)
            }
            Err(e) => Ok(failure(e)),
        }
    }
}

/// Images with more pixels than this are refused before decoding.
const MAX_OCR_PIXELS: u64 = 60_000_000;

/// How to make an oversized image readable.
const SHRINK_HINT: &str = "Shrink it to about 3000 px on the long side, then OCR the smaller file: for a PDF page `pdftoppm -scale-to 3000 -png -f N -l N <file.pdf> <prefix>`; for an image on macOS `sips -Z 3000 <in.png> --out <out.png>`.";

/// Width and height from the image header, without decoding the pixels.
fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()
}

/// OCR requests one page may have in flight: each fleet OCR node reads one
/// image at a time, so one page must not take most of the pool.
const STRIPS_IN_FLIGHT: usize = 3;

/// What to send for one page.
enum Reading {
    Whole(String),
    /// Strips of a tall page, top to bottom.
    Strips(Vec<String>),
    /// A sideways page turned 90° and 270° clockwise, each as (whole page,
    /// top part).
    Rotations([(String, String); 2]),
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{b64}")
}

/// Blocking: decodes the page to decide how to read it.
fn plan_reading(bytes: Vec<u8>, mime: String, plan_page: bool) -> Reading {
    use rsclaw_util::ocr_page::{OcrPlan, plan_ocr_page};
    let plan = if plan_page { plan_ocr_page(&bytes) } else { OcrPlan::Whole };
    match plan {
        OcrPlan::Strips(parts) => {
            Reading::Strips(parts.iter().map(|p| data_uri("image/jpeg", p)).collect())
        }
        OcrPlan::Rotations(turns) => Reading::Rotations(
            turns.map(|t| (data_uri("image/jpeg", &t.whole), data_uri("image/jpeg", &t.top))),
        ),
        OcrPlan::Whole => {
            // A page rendered at high DPI can pass the endpoint's 16 MB
            // body limit; OCR reads a 2400px long edge fine.
            let (bytes, mime) =
                rsclaw_util::downscale_image_for_vision(&bytes, &mime, 4 * 1024 * 1024, 2400, 90)
                    .unwrap_or((bytes, mime));
            Reading::Whole(data_uri(&mime, &bytes))
        }
    }
}

/// Output cap for one strip when the caller set none.
const STRIP_MAX_TOKENS: u32 = 2048;

/// Whether a reading is mostly one sentence said over and over — what some
/// OCR models produce for a blank or unreadable image ("The transcript is
/// too blurry to be identified. The transcript is …").
fn looping_text(text: &str) -> bool {
    let sentences: Vec<&str> = text
        .split(|c: char| matches!(c, '.' | '。' | '!' | '！' | '?' | '？' | '\n'))
        .map(str::trim)
        .filter(|s| s.chars().count() >= 8)
        .collect();
    if sentences.len() < 5 {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for s in &sentences {
        *counts.entry(*s).or_insert(0usize) += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    top >= 5 && top * 2 >= sentences.len()
}

#[derive(Clone)]
struct OcrRequest {
    client: std::sync::Arc<OcrClient>,
    prompt: Option<String>,
    max_tokens: Option<u32>,
    lang: Option<String>,
}

/// One image's text, and how long the endpoint took to produce it.
struct OcrRead {
    text: String,
    secs: f64,
}

impl OcrRequest {
    /// One image to text. A busy or failing endpoint gets a few backed-off
    /// retries first.
    async fn read(&self, payload: String) -> std::result::Result<OcrRead, String> {
        let _slot = ocr_slots()
            .acquire()
            .await
            .map_err(|e| format!("ocr: slot closed: {e}"))?;
        let client = self.client.clone();
        let (prompt, max_tokens, lang) = (self.prompt.clone(), self.max_tokens, self.lang.clone());
        // OCR transport is sync (block_in_place); run it off the async
        // worker so we don't stall the runtime thread on a slow page.
        tokio::task::spawn_blocking(move || {
            let mut attempt = 0;
            loop {
                let started = std::time::Instant::now();
                let result = client.ocr_with_options(
                    &payload,
                    prompt.as_deref(),
                    max_tokens,
                    None,
                    lang.as_deref(),
                );
                match result {
                    Err(e)
                        if attempt < OCR_RETRY_DELAYS_MS.len()
                            && is_server_failure(&format!("{e:#}")) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(
                            OCR_RETRY_DELAYS_MS[attempt],
                        ));
                        attempt += 1;
                    }
                    other => {
                        break other
                            .map(|text| OcrRead { text, secs: started.elapsed().as_secs_f64() })
                            .map_err(|e| format!("ocr request failed: {e:#}"));
                    }
                }
            }
        })
        .await
        .map_err(|e| format!("ocr: task join failed: {e}"))?
    }
}

/// The tool result for a failed read; trips the breaker after several
/// server-side failures in a row.
fn failure(error: String) -> Value {
    if is_server_failure(&error)
        && OCR_SERVER_FAILURES.fetch_add(1, Ordering::Relaxed) + 1 >= OCR_FAILURES_TO_TRIP
    {
        OCR_SERVER_FAILURES.store(0, Ordering::Relaxed);
        OCR_DOWN_UNTIL.store(now_secs() + OCR_DOWN_SECS, Ordering::Relaxed);
    }
    json!({
        "error": error,
        "hint": "Endpoint may be down or the image unreadable. Retry once; if it persists tell the user."
    })
}

/// Share of a reading's phrases that repeat an earlier one. Read at the
/// wrong angle, the model tends to loop on one phrase.
fn repeated_phrase_share(text: &str) -> f64 {
    let mut seen = std::collections::HashSet::new();
    let (mut phrases, mut repeats) = (0usize, 0usize);
    for phrase in text
        .split(|c: char| matches!(c, '；' | '、' | '，' | '。' | ';' | ',' | '\n'))
        .map(str::trim)
        .filter(|p| p.chars().count() >= 3)
    {
        phrases += 1;
        if !seen.insert(phrase) {
            repeats += 1;
        }
    }
    if phrases == 0 { 0.0 } else { repeats as f64 / phrases as f64 }
}

/// Character pairs (CJK and digits) of a reading, to compare readings.
fn char_pairs(text: &str) -> std::collections::HashSet<(char, char)> {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| c.is_ascii_digit() || ('\u{4E00}'..='\u{9FFF}').contains(c))
        .collect();
    chars.windows(2).map(|w| (w[0], w[1])).collect()
}

/// How much of the shorter reading the other one also contains.
fn agreement(a: &str, b: &str) -> f64 {
    let (a, b) = (char_pairs(a), char_pairs(b));
    let smaller = a.len().min(b.len());
    if smaller == 0 { 0.0 } else { a.intersection(&b).count() as f64 / smaller as f64 }
}

/// Of a sideways page turned 90° and 270°, the way that is upright: read
/// the right way up, the whole page and its top part say the same thing;
/// the wrong way up, the model invents text that changes with the crop.
/// (The invented text can be fluent, fast and loop-free, so it is the
/// disagreement that gives it away.)
fn pick_consistent(
    whole_90: OcrRead,
    top_90: OcrRead,
    whole_270: OcrRead,
    top_270: OcrRead,
) -> (String, u16) {
    let (a, b) = (agreement(&whole_90.text, &top_90.text), agreement(&whole_270.text, &top_270.text));
    tracing::info!(agree_90 = a, agree_270 = b, "ocr: sideways page");
    if (a - b).abs() < 0.1 {
        return pick_upright(whole_90, whole_270);
    }
    if a > b { (whole_90.text, 90) } else { (whole_270.text, 270) }
}

/// Of the readings of a page turned 90° and 270°, the one read upright:
/// the one that loops less, else the clearly faster one, else the shorter.
/// The wrong way up the model generates far more (a loop the endpoint
/// trims, or invented text), which shows as time more than as length.
fn pick_upright(turned_90: OcrRead, turned_270: OcrRead) -> (String, u16) {
    let (a, b) = (repeated_phrase_share(&turned_90.text), repeated_phrase_share(&turned_270.text));
    let (fast, slow) = (turned_90.secs.min(turned_270.secs), turned_90.secs.max(turned_270.secs));
    let first = if (a - b).abs() > 0.05 {
        a < b
    } else if slow > fast * 1.5 {
        turned_90.secs < turned_270.secs
    } else {
        turned_90.text.chars().count() <= turned_270.text.chars().count()
    };
    tracing::info!(
        repeats_90 = a,
        repeats_270 = b,
        secs_90 = turned_90.secs,
        secs_270 = turned_270.secs,
        chose = if first { 90 } else { 270 },
        "ocr: sideways page, readings agree alike"
    );
    if first { (turned_90.text, 90) } else { (turned_270.text, 270) }
}

fn parse_max_tokens(args: &Value) -> std::result::Result<Option<u32>, &'static str> {
    args["max_tokens"]
        .as_u64()
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "ocr: `max_tokens` must be between 0 and 4294967295")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_failures_are_told_apart_from_bad_images() {
        assert!(is_server_failure(
            "ocr request failed: HTTP status server error (502 Bad Gateway) for url (x)"
        ));
        assert!(!is_server_failure(
            "ocr request failed: HTTP status client error (413 Request Entity Too Large) for url (x)"
        ));
    }

    #[test]
    fn ocr_reads_image_size_from_the_header() {
        let dir = std::env::temp_dir().join("rsclaw_ocr_dims_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.png");
        image::RgbImage::new(300, 200).save(&path).unwrap();
        assert_eq!(image_dimensions(&path), Some((300, 200)));
        assert_eq!(image_dimensions(&dir.join("missing.png")), None);
    }

    #[test]
    fn ocr_spots_a_reading_that_loops() {
        let blurry = "The transcript is too blurry to be identified. ".repeat(40);
        assert!(looping_text(&blurry));
        let summons = "武汉市汉阳区人民法院\n传票\n案号 （2026）鄂0105民初16393号\n案由 中介合同纠纷\n\
            注意事项：\n1、被传唤人必须准时到达应到处所。\n2、本票由被传唤人携带来院报到。\n\
            3、原告经传票传唤，无正当理由拒不到庭的，可以按撤诉处理。";
        assert!(!looping_text(summons));
        assert!(!looping_text("短。"));
    }

    #[test]
    fn ocr_keeps_the_reading_that_does_not_loop() {
        let upright = "名称 武汉恒远建筑装饰工程有限公司\n经营范围 建筑工程劳务分包；建筑设备租赁；钢管扣件租赁；模板脚手架安装及租赁。";
        let looping = "名称 武汉德坤建筑装饰工程有限公司\n经营范围 土石方工程、园林绿化工程、土石方工程、园林绿化工程、土石方工程、园林绿化工程、土石方工程";
        assert!(repeated_phrase_share(looping) > 0.4);
        assert!(repeated_phrase_share(upright) < 0.05);
        let read = |text: &str, secs| OcrRead { text: text.into(), secs };
        assert_eq!(pick_upright(read(looping, 9.0), read(upright, 9.0)), (upright.to_string(), 270));
        assert_eq!(pick_upright(read(upright, 9.0), read(looping, 9.0)), (upright.to_string(), 90));
        // A trimmed loop leaves no repeats but took far longer.
        let invented = "名称 武汉市硚口区汉正街1号\n法定代表人 李明远";
        assert_eq!(pick_upright(read(invented, 29.5), read(upright, 8.4)), (upright.to_string(), 270));
    }

    #[test]
    fn ocr_keeps_the_way_up_whose_readings_agree() {
        let read = |text: &str| OcrRead { text: text.into(), secs: 8.0 };
        let whole = "统一社会信用代码 91420112MA4L12345X\n名称 武汉恒远建筑装饰工程有限公司\n法定代表人 李明远";
        let top = "统一社会信用代码 91420112MA4L12345X\n名称 武汉恒远建筑装饰工程有限公司";
        let invented = "统一社会信用代码 91420112MA4L12345X\n名称 武汉市硚口区汉正街汉正中路1号";
        let invented_top = "注册号 420112\n名称 武汉恒坤商贸有限公司\n住所 重庆市南岸区";
        assert!(agreement(whole, top) > 0.9);
        assert!(agreement(invented, invented_top) < 0.5);
        let (text, degrees) = pick_consistent(read(invented), read(invented_top), read(whole), read(top));
        assert_eq!((text.as_str(), degrees), (whole, 270));
    }

    #[test]
    fn max_tokens_rejects_values_that_do_not_fit_the_wire_type() {
        assert_eq!(
            parse_max_tokens(&json!({ "max_tokens": u32::MAX })),
            Ok(Some(u32::MAX))
        );
        assert!(parse_max_tokens(&json!({ "max_tokens": u64::from(u32::MAX) + 1 })).is_err());
    }
}
