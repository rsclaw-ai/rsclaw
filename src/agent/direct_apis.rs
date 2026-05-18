//! Direct API source configuration.
//!
//! `query_planner` recognises a handful of intents (weather, currency,
//! crypto price, github repo, …) that route to dedicated public endpoints
//! instead of through a generic web_search. Those URLs and the small
//! parser anchors that pin response shape (e.g. the `var fc40` prefix
//! that wraps `d1.weather.com.cn`'s JSON-in-JS payload) live in
//! `defaults.toml` under `[direct_apis.*]` so they can be swapped without
//! a code change when an upstream rebrands (jinse → jinse2, etc.) or a
//! mirror moves.
//!
//! Response **parsing logic** stays in code — only the volatile string
//! anchors and URL templates are externalised, per the design discussion
//! 2026-05-15 (see `docs/adr/*direct-apis*` if/when authored).
//!
//! Defaults are baked in so every fetch function has a non-None config
//! to read from even when the user has never edited `defaults.toml`.

use std::sync::OnceLock;

use serde::Deserialize;
use tracing::warn;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Top-level configuration: one struct per intent family.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DirectApisConfig {
    pub weather: WeatherApis,
    pub crypto: CryptoApis,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherApis {
    /// `weather.com.cn` (Chinese national weather service) — 15-day
    /// precise forecast + 40-day projection for CN cities. Two-step
    /// flow: search to resolve cityid, then pull the calendar payload.
    pub weather_cn: WeatherCnConfig,
    /// `api.open-meteo.com` — free 7-day forecast, lat/lon based,
    /// covers worldwide. Used when the agent has coordinates.
    pub openmeteo: UrlOnly,
    /// `wttr.in` — text/JSON weather, accepts city names in any
    /// language. 3-day forecast, no key. Universal fallback.
    pub wttr: UrlOnly,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherCnConfig {
    /// `{name}` template parameter — the city name (URL-encoded by the
    /// caller).
    pub city_search_url: String,
    /// `{year}` `{cityid}` `{yyyymm}` `{now_ms}` template parameters.
    pub calendar_url: String,
    /// Name of the JS variable the calendar response wraps its JSON in.
    /// Currently `fc40`; if the upstream renames it the parser needs
    /// this string to strip the right prefix.
    pub js_var_name: String,
    /// Referer header the upstream expects for cross-origin queries.
    pub referer: String,
    /// Cityid prefix that classifies a city as CN-resolvable. Foreign
    /// cities also resolve via search but their calendar endpoints 302,
    /// so the dispatcher bails before fetching if the resolved cityid
    /// doesn't start with this prefix. CN cityids start with "10"
    /// (provinces 01-99).
    pub cityid_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CryptoApis {
    /// `api.coingecko.com` — multi-coin, multi-currency, returns USD
    /// and CNY in a single call. Primary source.
    pub coingecko: UrlOnly,
    /// `min-api.cryptocompare.com` — single-coin lightweight quote.
    /// Backup when CoinGecko rate-limits or DNS-fails (both have been
    /// observed in CN networks at different times).
    pub cryptocompare: UrlOnly,
    /// `jinse2.com` — Chinese crypto news aggregator. News-only (not
    /// quotes). `{limit}` template parameter.
    pub jinse: JinseConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UrlOnly {
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JinseConfig {
    pub news_url: String,
}

// ---------------------------------------------------------------------------
// Built-in defaults — mirror the URLs the legacy hardcoded fetchers used.
// ---------------------------------------------------------------------------

impl Default for WeatherCnConfig {
    fn default() -> Self {
        Self {
            city_search_url: "https://toy1.weather.com.cn/search?cityname={name}".into(),
            calendar_url:
                "https://d1.weather.com.cn/calendar_new/{year}/{cityid}_{yyyymm}.html?_={now_ms}"
                    .into(),
            js_var_name: "fc40".into(),
            referer: "https://www.weather.com.cn/".into(),
            cityid_prefix: "10".into(),
        }
    }
}

impl Default for WeatherApis {
    fn default() -> Self {
        Self {
            weather_cn: WeatherCnConfig::default(),
            openmeteo: UrlOnly {
                url: "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
                      &daily=temperature_2m_max,temperature_2m_min,precipitation_sum,weathercode\
                      &forecast_days=7&timezone=Asia/Shanghai"
                    .into(),
            },
            wttr: UrlOnly {
                url: "https://wttr.in/{location}?format=j1".into(),
            },
        }
    }
}

impl Default for CryptoApis {
    fn default() -> Self {
        Self {
            coingecko: UrlOnly {
                url: "https://api.coingecko.com/api/v3/simple/price?\
                      ids={coin}&vs_currencies=usd,cny"
                    .into(),
            },
            cryptocompare: UrlOnly {
                url: "https://min-api.cryptocompare.com/data/price?fsym={symbol}&tsyms=USD,CNY"
                    .into(),
            },
            jinse: JinseConfig {
                news_url:
                    "https://api.jinse2.com/v6/information/list?catelogue_key=news&limit={limit}"
                        .into(),
            },
        }
    }
}

impl Default for DirectApisConfig {
    fn default() -> Self {
        Self {
            weather: WeatherApis::default(),
            crypto: CryptoApis::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Just enough of `defaults.toml`'s top-level shape to deserialise our
/// section. Other sections (`providers`, `channels`, `search_engines`,
/// …) are parsed by their respective owners; the `#[serde(default)]`s
/// here let us ignore them entirely.
#[derive(Debug, Deserialize, Default)]
struct DefaultsToml {
    #[serde(default)]
    direct_apis: DirectApisConfig,
}

impl DirectApisConfig {
    /// Parse `[direct_apis.*]` out of `defaults.toml`. Falls back to the
    /// built-in defaults (which match the legacy hardcoded URLs) on any
    /// parse error or when the section is absent. Logs a warning on
    /// parse failure so operators see the misconfig, but the gateway
    /// keeps booting.
    pub fn load() -> Self {
        let toml_text = crate::config::loader::load_defaults_toml();
        match toml::from_str::<DefaultsToml>(&toml_text) {
            Ok(d) => d.direct_apis,
            Err(e) => {
                warn!(error = %e, "defaults.toml: direct_apis parse failed, using built-in URLs");
                Self::default()
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Process-wide accessor
// ---------------------------------------------------------------------------

static DIRECT_APIS: OnceLock<DirectApisConfig> = OnceLock::new();

/// Return the process-wide direct-APIs configuration. Loaded lazily on
/// first access from `defaults.toml`; subsequent calls return the same
/// cached config — restart the gateway to reload after editing
/// `defaults.toml`. This matches the behaviour of providers/channels
/// (also baked at startup, hot-reload-exempt) and keeps the access
/// surface trivial so individual `tools_web` fetchers don't need a
/// config parameter threaded down from `AgentRuntime`.
pub fn config() -> &'static DirectApisConfig {
    DIRECT_APIS.get_or_init(DirectApisConfig::load)
}
