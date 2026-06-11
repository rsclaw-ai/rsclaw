//! K-line chart rendering for `tool_stock_chart` and the daily
//! briefing pipeline. Pure rendering — takes already-fetched bars
//! and writes a PNG. The caller decides where to save and how to
//! deliver (IM upload, artifact pipeline, Tauri UI).
//!
//! Style choices are A-share-specific:
//!   * **红涨绿跌** — opposite of US/Western convention. Hardcoded,
//!     because misrendering this confuses every Chinese reader at a
//!     glance.
//!   * MA5 / MA10 / MA20 / MA60 overlays — the default crosshair
//!     four 散户 grew up with.
//!   * Volume subplot under price, same coloring as the bar.
//!   * Title carries code + last close + change %.
//!
//! Pure-function design: no globals, no `&self`, all inputs explicit.
//! Easy to call from a cron task, the LLM tool, or a future Tauri
//! preview.

use std::path::Path;

use anyhow::{Context, Result};
use plotters::element::CandleStick;
use plotters::prelude::*;

use super::client::KlineBar;

/// A-share up color — bright red. Wechat / Feishu render this
/// against light backgrounds; pure `#FF0000` is too harsh, prefer
/// `#E74C3C`-ish.
const COLOR_UP: RGBColor = RGBColor(0xE7, 0x4C, 0x3C);
/// A-share down color — a forest-green that holds up against light
/// backgrounds and doesn't clash with the up-red.
const COLOR_DOWN: RGBColor = RGBColor(0x2E, 0xCC, 0x71);
const COLOR_BG: RGBColor = RGBColor(0xFA, 0xFA, 0xFA);
const COLOR_GRID: RGBColor = RGBColor(0xDD, 0xDD, 0xDD);
const COLOR_AXIS: RGBColor = RGBColor(0x88, 0x88, 0x88);

/// In-process logical font name. We register a real CJK-bearing
/// font file under this name at startup (see
/// `ensure_cjk_font_registered`), so referring to `CHART_FONT`
/// everywhere just routes to whatever we managed to load.
///
/// Why not `("PingFang SC", ...)` directly: plotters' font resolver
/// uses ab_glyph + a tiny in-memory map. It does NOT walk the OS
/// font catalog (no fontconfig / fontdb integration). So naming a
/// system font does nothing unless we've registered it ourselves
/// first. Verified on macOS — setting the family to "PingFang SC"
/// fell through to ab_glyph's no-glyph path and CJK chars silently
/// dropped from subtitles. Registering the file explicitly is the
/// only path that works.
const CHART_FONT: &str = "rsclaw-chart-cjk";

/// Try to register a system CJK font under `CHART_FONT`. Runs once
/// per process via `OnceLock`; subsequent calls are a noop.
///
/// Search order:
///   * macOS: `STHeiti Light.ttc` then `STHeiti Medium.ttc` —
///     ship with every macOS install since 10.11. They're TTCs
///     (font collections) but ab_glyph's `try_from_slice` accepts
///     the first face inside them, which is plenty for chart
///     subtitle / tick labels (we don't need bold/italic).
///   * Windows: `C:\Windows\Fonts\msyh.ttc` (Microsoft YaHei) and
///     `simhei.ttf` (SimHei) as a fallback.
///   * Linux: Noto Sans CJK SC / WenQuanYi Micro Hei at common
///     fontconfig install paths.
///
/// Returns `true` if a font was successfully registered, `false`
/// if every candidate failed (e.g. minimal Linux containers).
/// Caller's behaviour when registration fails: chart still renders,
/// CJK glyphs silently drop — same as the pre-fix state, so no
/// regression for systems we couldn't help.
fn ensure_cjk_font_registered() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| {
        let candidates: &[&str] = {
            #[cfg(target_os = "macos")]
            {
                // Single-face TTFs FIRST — ab_glyph's `try_from_slice`
                // ostensibly accepts the first face of a TTC, but the
                // first run against `STHeiti Light.ttc` registered
                // cleanly yet still dropped Chinese glyphs at render
                // time (probably ab_glyph parsing the TTC header as
                // a corrupt single-face TTF). Arial Unicode.ttf is a
                // 23 MB single-face TTF covering CJK + every other
                // Unicode block, shipped at `/Library/Fonts/` on every
                // modern macOS install. Falling back to the TTCs only
                // when that path is missing — better than nothing on
                // systems that don't ship Arial Unicode.
                &[
                    "/Library/Fonts/Arial Unicode.ttf",
                    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
                    "/System/Library/Fonts/STHeiti Light.ttc",
                    "/System/Library/Fonts/STHeiti Medium.ttc",
                    "/System/Library/Fonts/Supplemental/Songti.ttc",
                ]
            }
            #[cfg(target_os = "windows")]
            {
                &[
                    "C:\\Windows\\Fonts\\msyh.ttc",
                    "C:\\Windows\\Fonts\\msyh.ttf",
                    "C:\\Windows\\Fonts\\simhei.ttf",
                    "C:\\Windows\\Fonts\\simsun.ttc",
                ]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                &[
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
                    "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
                    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
                ]
            }
        };
        for path in candidates {
            match std::fs::read(path) {
                Ok(bytes) => {
                    // plotters' `register_font` wants a `&'static [u8]`.
                    // We never UNregister within the process lifetime, so
                    // a one-shot `Box::leak` is the cleanest path — the
                    // leaked memory persists exactly as long as plotters
                    // needs it, no Arc/Rc bookkeeping required.
                    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                    match plotters::style::register_font(
                        CHART_FONT,
                        FontStyle::Normal,
                        leaked,
                    ) {
                        Ok(()) => {
                            tracing::info!(font = path, "chart: CJK font registered");
                            return true;
                        }
                        Err(_) => {
                            tracing::debug!(font = path, "chart: register_font rejected (likely TTC face-0 mismatch); trying next");
                            // Leaked bytes stay allocated; we tried.
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        tracing::warn!(
            "chart: no CJK font found at known paths; subtitles/axis labels may drop Chinese glyphs"
        );
        false
    })
}
const COLOR_TITLE: RGBColor = RGBColor(0x33, 0x33, 0x33);

// MA line colors. All four must contrast against the `#FAFAFA` light
// background AND against the red/green candle bodies — the first cut
// painted MA5 white (`#FFFFFF`), which the live-feishu screenshot
// proved was invisible on the chart. Replaced with a warm grey that
// still reads as "muted" against the louder MA10/20/60 colors but is
// actually drawable. Order matches 散户 conventions:
//   MA5  灰白 (now visible)
//   MA10 黄
//   MA20 紫
//   MA60 蓝
const COLOR_MA5: RGBColor = RGBColor(0x55, 0x55, 0x55);
const COLOR_MA10: RGBColor = RGBColor(0xE6, 0xB4, 0x22);
const COLOR_MA20: RGBColor = RGBColor(0x8E, 0x44, 0xAD);
const COLOR_MA60: RGBColor = RGBColor(0x29, 0x80, 0xB9);

/// Options for `render_kline_png`. Defaults are tuned for IM
/// delivery (800×500 fits feishu/wechat preview without scaling).
#[derive(Debug, Clone)]
pub struct ChartOpts {
    pub width: u32,
    pub height: u32,
    /// Header line — usually `"<name> <code>  ¥<price>  +X.XX%"`.
    /// Caller composes the string so this module stays unaware of
    /// stock_name lookups.
    pub title: String,
    /// Subtitle (e.g. `"日线 · 近 60 根 · MA5/10/20/60"`).
    pub subtitle: Option<String>,
    /// Which MA overlays to draw. Empty disables overlays
    /// completely (raw candlesticks only).
    pub ma_periods: Vec<usize>,
}

impl Default for ChartOpts {
    fn default() -> Self {
        Self {
            width: 800,
            height: 500,
            title: String::new(),
            subtitle: None,
            ma_periods: vec![5, 10, 20, 60],
        }
    }
}

/// Render a K-line + volume PNG to `path`. Returns the file size
/// in bytes on success. Bars are expected to be in ascending time
/// order; we plot them as-is.
pub fn render_kline_png<P: AsRef<Path>>(
    bars: &[KlineBar],
    opts: &ChartOpts,
    path: P,
) -> Result<u64> {
    if bars.is_empty() {
        anyhow::bail!("render_kline_png: no bars supplied");
    }
    // First call per-process loads the CJK font; subsequent calls
    // are a OnceLock-guarded noop. Cheaper to inline here than to
    // require every caller (cron, LLM tool, future Tauri preview)
    // to remember a separate init step.
    let _ = ensure_cjk_font_registered();
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create chart parent {}", parent.display()))?;
    }

    let root = BitMapBackend::new(path, (opts.width, opts.height)).into_drawing_area();
    root.fill(&COLOR_BG).context("fill background")?;

    // Title strip (top ~36px). Use plotters' text drawing directly
    // rather than the built-in caption — captions only appear on
    // ChartBuilder, and we want the title above the split between
    // price + volume panes.
    let (title_area, body) = root.split_vertically(40);
    title_area
        .titled(
            &opts.title,
            (CHART_FONT, 22).into_font().color(&COLOR_TITLE),
        )
        .context("title")?;
    if let Some(sub) = &opts.subtitle {
        let sub_y = 24;
        title_area
            .draw_text(
                sub,
                &(CHART_FONT, 12).into_font().color(&COLOR_AXIS),
                (10, sub_y),
            )
            .context("subtitle")?;
    }

    // Split body 75% price / 25% volume — standard for 散户 charts.
    let body_h = opts.height.saturating_sub(40);
    let price_h = (body_h * 3) / 4;
    let (price_area, vol_area) = body.split_vertically(price_h);

    // ---- Price + candlesticks ---------------------------------------
    let (lo, hi) = price_range(bars);
    let pad = (hi - lo) * 0.05;
    let y_lo = lo - pad;
    let y_hi = hi + pad;
    let n = bars.len() as i32;

    let mut chart = ChartBuilder::on(&price_area)
        .margin(8)
        .x_label_area_size(0) // volume axis below carries the dates
        .y_label_area_size(60)
        .build_cartesian_2d(0i32..n, y_lo..y_hi)
        .context("price chart")?;

    chart
        .configure_mesh()
        .x_labels(0)
        .y_labels(6)
        .light_line_style(COLOR_GRID.mix(0.5))
        .bold_line_style(COLOR_GRID)
        .axis_style(COLOR_AXIS)
        .label_style((CHART_FONT, 11).into_font().color(&COLOR_AXIS))
        .y_label_formatter(&|y| format!("{y:.2}"))
        .draw()
        .context("price mesh")?;

    chart
        .draw_series(bars.iter().enumerate().map(|(i, b)| {
            // CandleStick::new(x, open, high, low, close, up_style, down_style, width).
            // The Y param matches the chart's y-axis type (f64 here so
            // we keep price precision); width is in chart x-units (4 px
            // wide bars hold up at typical 60-bar density).
            CandleStick::new(
                i as i32,
                b.open,
                b.high,
                b.low,
                b.close,
                COLOR_UP.filled(),
                COLOR_DOWN.filled(),
                4,
            )
        }))
        .context("candlesticks")?;

    // ---- Current-price horizontal hint ------------------------------
    // Dashed line at the last bar's close — common Chinese-broker
    // chart convention. Helps the eye find "where we are now"
    // without having to read the tick label. Uses the up/down
    // colour of the last bar so the colour itself encodes whether
    // today is green or red versus the prior close.
    if let Some(last) = bars.last() {
        let line_color = if last.close >= last.open {
            COLOR_UP
        } else {
            COLOR_DOWN
        };
        chart
            .draw_series(std::iter::once(PathElement::new(
                vec![(0i32, last.close), (n, last.close)],
                line_color.mix(0.45).stroke_width(1),
            )))
            .context("current-price line")?;
    }

    // ---- MA overlays ------------------------------------------------
    for (idx, &period) in opts.ma_periods.iter().enumerate() {
        if period < 2 || bars.len() < period {
            continue;
        }
        let color = ma_color(idx, period);
        let ma = simple_ma(bars, period);
        chart
            .draw_series(LineSeries::new(
                ma.iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.map(|y| (i as i32, y))),
                color.stroke_width(1),
            ))
            .with_context(|| format!("MA{period}"))?
            .label(format!("MA{period}"))
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 14, y)], color.stroke_width(2)));
    }
    // Legend in the UPPER-RIGHT — the first cut put it in UpperLeft
    // which overlaps the candle series for any chart whose earliest
    // bars are near the top of the price range (verified against the
    // 600519 screenshot the user sent from feishu). UpperRight is
    // empty on >90% of charts because the most recent bars are
    // rarely also the chart's highest point. Slight white background
    // opacity (0.92) so wicks visible behind the box still read.
    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperRight)
        .background_style(COLOR_BG.mix(0.92))
        .border_style(COLOR_GRID)
        .label_font((CHART_FONT, 11).into_font().color(&COLOR_TITLE))
        .draw()
        .context("legend")?;

    // ---- Volume subplot --------------------------------------------
    let max_vol = bars.iter().map(|b| b.volume).max().unwrap_or(0).max(1);
    // Pad volume axis 10% so the tallest bar doesn't touch the top.
    let vol_top = (max_vol as f64 * 1.1) as i64;

    let mut vol_chart = ChartBuilder::on(&vol_area)
        .margin(8)
        .x_label_area_size(28)
        .y_label_area_size(60)
        .build_cartesian_2d(0i32..n, 0i64..vol_top)
        .context("vol chart")?;

    vol_chart
        .configure_mesh()
        // 10 ticks across 60-bar charts gives ≈ one per 6 bars,
        // which lines up with weekly granularity (5 trading days)
        // without the labels colliding. 6 ticks (the prior cut)
        // skipped enough that the user had to count bars to find
        // a date — confirmed against the 600519 chart shared this
        // morning where labels read 03-13 / 03-27 / 04-13 / 04-27.
        .x_labels(10)
        .y_labels(3)
        .light_line_style(COLOR_GRID.mix(0.5))
        .bold_line_style(COLOR_GRID)
        .axis_style(COLOR_AXIS)
        .label_style((CHART_FONT, 11).into_font().color(&COLOR_AXIS))
        .x_label_formatter(&|x| label_x(bars, *x))
        .y_label_formatter(&|v| compact_volume(*v))
        .draw()
        .context("vol mesh")?;

    vol_chart
        .draw_series(bars.iter().enumerate().map(|(i, b)| {
            let color = if b.close >= b.open {
                COLOR_UP.filled()
            } else {
                COLOR_DOWN.filled()
            };
            // 6px wide bars; centered on integer x.
            Rectangle::new([(i as i32, 0), (i as i32 + 1, b.volume)], color)
        }))
        .context("vol bars")?;

    // Force the in-memory buffer to flush before we measure the file.
    root.present().context("present")?;
    drop(root);

    let len = std::fs::metadata(path)
        .map(|m| m.len())
        .with_context(|| format!("stat {}", path.display()))?;
    Ok(len)
}

// --- helpers --------------------------------------------------------------

fn price_range(bars: &[KlineBar]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for b in bars {
        if b.low < lo {
            lo = b.low;
        }
        if b.high > hi {
            hi = b.high;
        }
    }
    if !lo.is_finite() || !hi.is_finite() || lo >= hi {
        // Pathological input: degenerate range. Return a small
        // ±5% window around the first close so plotters has
        // something to draw against.
        let anchor = bars.first().map(|b| b.close).unwrap_or(0.0);
        return ((anchor * 0.95).abs(), (anchor * 1.05).abs().max(0.01));
    }
    (lo, hi)
}

/// Simple moving average over the closes. `ma[i]` is `None` for
/// indices where the window has not yet filled.
fn simple_ma(bars: &[KlineBar], period: usize) -> Vec<Option<f64>> {
    if period < 2 {
        return bars.iter().map(|b| Some(b.close)).collect();
    }
    let mut out = vec![None; bars.len()];
    if bars.len() < period {
        return out;
    }
    let mut sum: f64 = bars.iter().take(period).map(|b| b.close).sum();
    out[period - 1] = Some(sum / period as f64);
    for i in period..bars.len() {
        sum += bars[i].close - bars[i - period].close;
        out[i] = Some(sum / period as f64);
    }
    out
}

fn ma_color(slot: usize, period: usize) -> RGBColor {
    // Period-aware first (so the trader sees the conventional color
    // even if periods are reordered), slot index as a fallback.
    match period {
        5 => COLOR_MA5,
        10 => COLOR_MA10,
        20 => COLOR_MA20,
        60 => COLOR_MA60,
        _ => match slot % 4 {
            0 => COLOR_MA5,
            1 => COLOR_MA10,
            2 => COLOR_MA20,
            _ => COLOR_MA60,
        },
    }
}

fn label_x(bars: &[KlineBar], i: i32) -> String {
    if i < 0 || (i as usize) >= bars.len() {
        return String::new();
    }
    let ts = bars[i as usize].timestamp;
    // astock timestamps come in seconds (tdx.Kline.Timestamp). Render
    // YYYY-MM-DD for daily-and-up periods.
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%m-%d").to_string())
        .unwrap_or_default()
}

/// Compact a large volume number as `1.2M` / `345K` / `999`.
/// astock's volume is in board-lots (`手`) which scales nicely.
fn compact_volume(v: i64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1_000 {
        format!("{:.1}K", v as f64 / 1_000.0)
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: i64, open: f64, high: f64, low: f64, close: f64, volume: i64) -> KlineBar {
        KlineBar {
            timestamp: ts,
            open,
            high,
            low,
            close,
            volume,
            amount: 0.0,
        }
    }

    #[test]
    fn price_range_basic() {
        let bars = vec![bar(0, 10.0, 12.0, 9.0, 11.0, 0), bar(1, 11.0, 13.0, 10.5, 12.0, 0)];
        let (lo, hi) = price_range(&bars);
        assert!((lo - 9.0).abs() < 1e-9);
        assert!((hi - 13.0).abs() < 1e-9);
    }

    #[test]
    fn price_range_degenerate_fallback() {
        let bars = vec![bar(0, 10.0, 10.0, 10.0, 10.0, 0)];
        let (lo, hi) = price_range(&bars);
        // Degenerate flat input: lo == hi triggers fallback to
        // a non-zero range around the close.
        assert!(lo < hi, "fallback must produce a non-empty range");
    }

    #[test]
    fn ma_window_alignment() {
        let bars: Vec<KlineBar> = (1..=10)
            .map(|i| bar(i, 0.0, 0.0, 0.0, i as f64, 0))
            .collect();
        let ma3 = simple_ma(&bars, 3);
        assert_eq!(ma3.len(), 10);
        assert!(ma3[0].is_none());
        assert!(ma3[1].is_none());
        assert_eq!(ma3[2], Some(2.0)); // (1+2+3)/3
        assert_eq!(ma3[3], Some(3.0));
        assert_eq!(ma3[9], Some(9.0)); // (8+9+10)/3
    }

    #[test]
    fn ma_short_history_returns_none() {
        let bars: Vec<KlineBar> = (1..=4)
            .map(|i| bar(i, 0.0, 0.0, 0.0, i as f64, 0))
            .collect();
        let ma10 = simple_ma(&bars, 10);
        assert!(ma10.iter().all(|v| v.is_none()));
    }

    #[test]
    fn compact_volume_thresholds() {
        assert_eq!(compact_volume(0), "0");
        assert_eq!(compact_volume(999), "999");
        assert_eq!(compact_volume(1_500), "1.5K");
        assert_eq!(compact_volume(2_300_000), "2.3M");
    }

    #[test]
    fn ma_color_period_aware() {
        // Slot 0 with period=20 should still get MA20's purple, not MA5's grey.
        assert_eq!(ma_color(0, 20), COLOR_MA20);
        assert_eq!(ma_color(0, 5), COLOR_MA5);
        // Unknown period falls back to slot-based.
        assert_eq!(ma_color(0, 13), COLOR_MA5);
        assert_eq!(ma_color(1, 13), COLOR_MA10);
    }

    #[test]
    fn renders_to_disk() {
        // Tiny render to confirm plotters resolves a font and writes a
        // non-empty PNG. Skipped if HOME isn't writable (CI sandbox).
        let bars: Vec<KlineBar> = (1..=30)
            .map(|i| {
                let close = 100.0 + (i as f64 * 0.3).sin();
                bar(
                    1_700_000_000 + (i as i64) * 86_400,
                    close - 0.5,
                    close + 0.7,
                    close - 0.8,
                    close,
                    100_000 + (i as i64) * 1_000,
                )
            })
            .collect();
        let path = std::env::temp_dir().join(format!("rsclaw_chart_test_{}.png", std::process::id()));
        let opts = ChartOpts {
            title: "Test 600519  ¥100.05  +0.50%".into(),
            subtitle: Some("日线 · 30 根 · MA5/10/20".into()),
            ma_periods: vec![5, 10, 20],
            ..Default::default()
        };
        let size = render_kline_png(&bars, &opts, &path).expect("render ok");
        assert!(size > 1000, "PNG should be at least 1KB, got {size}");
        let _ = std::fs::remove_file(&path);
    }
}
