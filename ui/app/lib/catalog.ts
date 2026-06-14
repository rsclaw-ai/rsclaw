// ─────────────────────────────────────────────────────────────────────────
// Catalog loader — provider / channel / search-engine definitions sourced
// from defaults.toml (Steps 5-6 of making the desktop UI fully
// defaults.toml-driven).
//
// The backend exposes the SAME catalog two ways, both snake_case:
//   1. Tauri command  `read_defaults_catalog`  → returns a JSON STRING
//      (works *before* the gateway starts — onboarding needs this).
//   2. Gateway HTTP    GET /api/v1/defaults      → returns JSON.
//
// This module loads that catalog once (module-level cached promise), maps the
// snake_case backend shape into the camelCase shapes the existing components
// already expect, and exposes typed accessors so consumer code barely changes.
//
// On ANY failure (no Tauri, gateway down, parse error) we fall back to
// FALLBACK_CATALOG — a verbatim copy of the previously-hardcoded data that
// lived in components/onboarding.tsx — so nothing ever regresses or renders
// blank.
//
// grok / xai reconciliation: the backend provider/channel `name` is `xai`.
// The old frontend id was `grok`. We STANDARDIZE on `xai` (backend wins) but
// keep a tiny back-compat alias so any lingering `grok` lookup still resolves
// to the same definition. See PROVIDER_ALIASES below.
// ─────────────────────────────────────────────────────────────────────────

import { useEffect, useState } from "react";
import { isTauriRuntime, invoke } from "../utils/tauri";
import { gatewayFetch } from "./rsclaw-api";

// ── camelCase shapes the components consume ──────────────────────────────

export interface CatalogProviderDef {
  id: string;
  name: string;
  tag: string;
  tagEn: string;
  keyLabel: string;
  keyPlaceholder: string;
  isUrl?: boolean;
  hasBaseUrl?: boolean;
  defaultBaseUrl?: string;
  defaultUserAgent?: string;
}

export interface CatalogChannelCredField {
  key: string;
  label: string;
  type: string; // "text" | "password"
  ph: string;
}

export interface CatalogChannelDef {
  id: string;
  icon: string;
  name: string;
  nameEn: string;
  hasQr: boolean;
  qrLabel?: string;
  qrLabelEn?: string;
  credFields: CatalogChannelCredField[];
}

export interface CatalogSearchEngineDef {
  value: string;
  label: string;
}

export interface Catalog {
  providers: Record<string, CatalogProviderDef>;
  providerOrderZh: string[];
  providerOrderEn: string[];
  channels: Record<string, CatalogChannelDef>;
  channelOrderZh: string[];
  channelOrderEn: string[];
  searchEnginesZh: CatalogSearchEngineDef[];
  searchEnginesEn: CatalogSearchEngineDef[];
}

// ── Raw backend (snake_case) shapes ──────────────────────────────────────

interface RawProvider {
  name: string;
  label?: string;
  env_var?: string;
  model?: string;
  base_url?: string;
  user_agent?: string;
  needs_key?: boolean;
  api_type?: string;
  name_zh?: string;
  name_en?: string;
  tag_zh?: string;
  tag_en?: string;
  key_label?: string;
  key_placeholder?: string;
  is_url?: boolean;
  has_base_url?: boolean;
  order_zh?: number;
  order_en?: number;
}

interface RawChannelField {
  key: string;
  prompt?: string;
  secret?: boolean;
  placeholder?: string;
}

interface RawChannel {
  name: string;
  label?: string;
  login?: boolean;
  multi_account?: boolean;
  fields?: RawChannelField[];
  name_zh?: string;
  icon?: string;
  has_qr?: boolean;
  qr_label_zh?: string;
  qr_label_en?: string;
  order_zh?: number;
  order_en?: number;
}

interface RawSearchEngine {
  name: string;
  label?: string;
  url?: string;
  env_var?: string;
  label_zh?: string;
  label_en?: string;
  needs_key?: boolean;
  order?: number;
}

interface RawCatalog {
  providers?: RawProvider[];
  channels?: RawChannel[];
  search_engines?: RawSearchEngine[];
}

// ── grok / xai back-compat alias ─────────────────────────────────────────
// Standardize on the backend id `xai`; keep `grok` resolving to it.
const PROVIDER_ALIASES: Record<string, string> = { grok: "xai" };

/** Resolve an alias id (e.g. "grok") to its canonical id (e.g. "xai"). */
export function canonicalProviderId(id: string): string {
  return PROVIDER_ALIASES[id] ?? id;
}

// ── Mapping snake_case → camelCase ───────────────────────────────────────

function mapProvider(p: RawProvider): CatalogProviderDef {
  const def: CatalogProviderDef = {
    id: p.name,
    name: p.name_zh || p.label || p.name,
    tag: p.tag_zh || "",
    tagEn: p.tag_en || "",
    keyLabel: p.key_label || "",
    keyPlaceholder: p.key_placeholder || "",
  };
  if (p.is_url) def.isUrl = true;
  if (p.has_base_url) def.hasBaseUrl = true;
  if (p.base_url) def.defaultBaseUrl = p.base_url;
  if (p.user_agent) def.defaultUserAgent = p.user_agent;
  return def;
}

function mapChannel(c: RawChannel): CatalogChannelDef {
  const def: CatalogChannelDef = {
    id: c.name,
    icon: c.icon || "",
    name: c.name_zh || c.label || c.name,
    nameEn: c.label || c.name,
    hasQr: !!c.has_qr,
    credFields: (c.fields || []).map((f) => ({
      key: f.key,
      label: f.prompt || "",
      type: f.secret ? "password" : "text",
      ph: f.placeholder || "",
    })),
  };
  if (c.qr_label_zh) def.qrLabel = c.qr_label_zh;
  if (c.qr_label_en) def.qrLabelEn = c.qr_label_en;
  return def;
}

function sortByOrder<T>(items: T[], orderOf: (t: T) => number, idOf: (t: T) => string): string[] {
  return [...items]
    .sort((a, b) => orderOf(a) - orderOf(b))
    .map(idOf);
}

function mapCatalog(raw: RawCatalog): Catalog {
  const providersArr = raw.providers || [];
  const channelsArr = raw.channels || [];
  const searchArr = raw.search_engines || [];

  const providers: Record<string, CatalogProviderDef> = {};
  for (const p of providersArr) providers[p.name] = mapProvider(p);
  // Expose aliases too (e.g. grok → xai's def) so lookups never miss.
  for (const [alias, canonical] of Object.entries(PROVIDER_ALIASES)) {
    if (providers[canonical] && !providers[alias]) providers[alias] = providers[canonical];
  }

  const channels: Record<string, CatalogChannelDef> = {};
  for (const c of channelsArr) channels[c.name] = mapChannel(c);

  const providerOrderZh = sortByOrder(providersArr, (p) => p.order_zh ?? 0, (p) => p.name);
  const providerOrderEn = sortByOrder(providersArr, (p) => p.order_en ?? 0, (p) => p.name);
  const channelOrderZh = sortByOrder(channelsArr, (c) => c.order_zh ?? 0, (c) => c.name);
  const channelOrderEn = sortByOrder(channelsArr, (c) => c.order_en ?? 0, (c) => c.name);

  const sortedSearch = [...searchArr].sort((a, b) => (a.order ?? 0) - (b.order ?? 0));
  const searchEnginesZh: CatalogSearchEngineDef[] = sortedSearch.map((s) => ({
    value: s.name,
    label: s.label_zh || s.label || s.name,
  }));
  const searchEnginesEn: CatalogSearchEngineDef[] = sortedSearch.map((s) => ({
    value: s.name,
    label: s.label_en || s.label || s.name,
  }));

  return {
    providers,
    providerOrderZh,
    providerOrderEn,
    channels,
    channelOrderZh,
    channelOrderEn,
    searchEnginesZh,
    searchEnginesEn,
  };
}

// ── FALLBACK_CATALOG ─────────────────────────────────────────────────────
// Verbatim copy of the previously-hardcoded data from
// components/onboarding.tsx (ALL_PROVIDERS / PROV_ORDER_* / ALL_CHANNELS /
// CH_ORDER_*) plus the search-engine <select> options that lived inline in
// components/rsclaw-panel.tsx. Already in the camelCase accessor shape, so it
// is used directly (no mapping) whenever the backend read fails.

const FALLBACK_PROVIDERS: Record<string, CatalogProviderDef> = {
  rsclaw:      { id: "rsclaw",      name: "RsClaw",             tag: "增量协议 · 业内首发", tagEn: "Incremental · Industry-first", keyLabel: "RsClaw API Key", keyPlaceholder: "sk-...", hasBaseUrl: true, defaultBaseUrl: "https://api.rsclaw.ai/v1/agent" },
  agnes:       { id: "agnes",       name: "Agnes AI",           tag: "免费 · 文图视", tagEn: "Free · text/image/video", keyLabel: "Agnes API Key", keyPlaceholder: "sk-...", hasBaseUrl: true, defaultBaseUrl: "https://apihub.agnes-ai.com/v1" },
  qwen:        { id: "qwen",        name: "Qwen (千问)", tag: "国内直连",      tagEn: "China direct",      keyLabel: "DashScope API Key",   keyPlaceholder: "sk-..." },
  doubao:      { id: "doubao",      name: "Doubao (豆包)", tag: "字节跳动",     tagEn: "ByteDance",         keyLabel: "ARK API Key",         keyPlaceholder: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx", hasBaseUrl: true, defaultBaseUrl: "https://ark.cn-beijing.volces.com/api/v3" },
  minimax:     { id: "minimax",     name: "MiniMax",            tag: "国内",                  tagEn: "China",             keyLabel: "MiniMax API Key",     keyPlaceholder: "eyJ..." },
  deepseek:    { id: "deepseek",    name: "DeepSeek",           tag: "低成本",            tagEn: "Low cost",          keyLabel: "DeepSeek API Key",    keyPlaceholder: "sk-..." },
  kimi:        { id: "kimi",        name: "Kimi",               tag: "国内",                  tagEn: "China",             keyLabel: "Kimi API Key",        keyPlaceholder: "sk-...", hasBaseUrl: true, defaultBaseUrl: "https://api.moonshot.cn/v1", defaultUserAgent: "rsclaw/2026.5.5" },
  codingplan:  { id: "codingplan",  name: "CodingPlan",         tag: "编程计划",      tagEn: "Coding Plan",       keyLabel: "API URL",             keyPlaceholder: "https://api.example.com/v1", isUrl: true },
  zhipu:       { id: "zhipu",       name: "Zhipu (GLM)",        tag: "国内",                  tagEn: "China",             keyLabel: "Zhipu API Key",       keyPlaceholder: "sk-..." },
  ollama:      { id: "ollama",      name: "Ollama (local)",     tag: "无需 Key",              tagEn: "No Key",            keyLabel: "URL",                 keyPlaceholder: "http://localhost:11434/v1", isUrl: true },
  custom:      { id: "custom",      name: "Custom Provider",    tag: "自定义",            tagEn: "Custom",            keyLabel: "API URL",             keyPlaceholder: "https://api.example.com/v1", isUrl: true },
  gaterouter:  { id: "gaterouter",  name: "GateRouter",         tag: "聚合路由",      tagEn: "Aggregator",        keyLabel: "GateRouter Key",      keyPlaceholder: "sk-..." },
  openrouter:  { id: "openrouter",  name: "OpenRouter",         tag: "聚合路由",      tagEn: "Aggregator",        keyLabel: "OpenRouter Key",      keyPlaceholder: "sk-or-..." },
  anthropic:   { id: "anthropic",   name: "Anthropic (Claude)", tag: "推荐",                  tagEn: "Recommended",       keyLabel: "Anthropic API Key",   keyPlaceholder: "sk-ant-..." },
  openai:      { id: "openai",      name: "OpenAI (GPT)",       tag: "",                              tagEn: "",                  keyLabel: "OpenAI API Key",      keyPlaceholder: "sk-..." },
  gemini:      { id: "gemini",      name: "Google Gemini",      tag: "",                              tagEn: "",                  keyLabel: "Gemini API Key",      keyPlaceholder: "AIza..." },
  // Backend id is `xai`; we standardize on it. `grok` alias added below.
  xai:         { id: "xai",         name: "xAI (Grok)",         tag: "",                              tagEn: "",                  keyLabel: "xAI API Key",         keyPlaceholder: "xai-..." },
  groq:        { id: "groq",        name: "Groq",               tag: "快速推理",      tagEn: "Fast",              keyLabel: "Groq API Key",        keyPlaceholder: "gsk_..." },
  siliconflow: { id: "siliconflow", name: "SiliconFlow",        tag: "国内加速",      tagEn: "China accel",       keyLabel: "SiliconFlow Key",     keyPlaceholder: "sk-..." },
};
// grok → xai alias (back-compat).
FALLBACK_PROVIDERS.grok = FALLBACK_PROVIDERS.xai;

// Old order arrays used `grok`; remap to `xai` to match the canonical id.
const FALLBACK_PROV_ORDER_ZH = ["rsclaw","agnes","qwen","deepseek","doubao","custom","codingplan","minimax","kimi","zhipu","ollama","gaterouter","openrouter","anthropic","openai","gemini","xai","groq","siliconflow"];
const FALLBACK_PROV_ORDER_EN = ["rsclaw","agnes","anthropic","openai","gemini","xai","openrouter","ollama","custom","codingplan","groq","doubao","qwen","minimax","deepseek","kimi","zhipu","gaterouter","siliconflow"];

const FALLBACK_CHANNELS: Record<string, CatalogChannelDef> = {
  feishu:   { id: "feishu",   icon: "飞", name: "飞书 / Lark", nameEn: "Feishu / Lark",  hasQr: true, qrLabel: "扫码", qrLabelEn: "QR", credFields: [
    { key: "appId", label: "App ID", type: "text", ph: "cli_xxx" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  wechat:   { id: "wechat",   icon: "微", name: "微信",        nameEn: "Weixin",         hasQr: true, qrLabel: "扫码", qrLabelEn: "QR", credFields: [] },
  wecom:    { id: "wecom",    icon: "WC",     name: "企业微信", nameEn: "WeCom",     hasQr: false, credFields: [
    { key: "botId", label: "Bot ID", type: "text", ph: "" },
    { key: "secret", label: "Secret", type: "password", ph: "" },
  ] },
  qq:       { id: "qq",       icon: "QQ",     name: "QQ Bot",              nameEn: "QQ Bot",          hasQr: false, credFields: [
    { key: "appId", label: "App ID", type: "text", ph: "" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  dingtalk: { id: "dingtalk", icon: "DT",     name: "钉钉",        nameEn: "DingTalk",        hasQr: false, credFields: [
    { key: "appKey", label: "App Key", type: "text", ph: "" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  telegram: { id: "telegram", icon: "Tg",     name: "Telegram",            nameEn: "Telegram",        hasQr: false, credFields: [
    { key: "botToken", label: "Bot Token", type: "password", ph: "123456:ABC-DEF..." },
  ] },
  matrix:   { id: "matrix",   icon: "Mx",     name: "Matrix",              nameEn: "Matrix",          hasQr: false, credFields: [
    { key: "homeserver", label: "Homeserver", type: "text", ph: "https://matrix.org" },
    { key: "userId", label: "User ID", type: "text", ph: "@bot:matrix.org" },
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  discord:  { id: "discord",  icon: "Dc",     name: "Discord",             nameEn: "Discord",         hasQr: false, credFields: [
    { key: "token", label: "Bot Token", type: "password", ph: "" },
  ] },
  slack:    { id: "slack",    icon: "Sl",     name: "Slack",               nameEn: "Slack",           hasQr: false, credFields: [
    { key: "botToken", label: "Bot Token", type: "password", ph: "xoxb-..." },
    { key: "appToken", label: "App Token", type: "password", ph: "xapp-..." },
  ] },
  whatsapp: { id: "whatsapp", icon: "WA",     name: "WhatsApp",            nameEn: "WhatsApp",        hasQr: false, credFields: [
    { key: "phoneNumberId", label: "Phone Number ID", type: "text", ph: "" },
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  signal:   { id: "signal",   icon: "Sg",     name: "Signal",              nameEn: "Signal",          hasQr: false, credFields: [
    { key: "phone", label: "Phone Number", type: "text", ph: "+1234567890" },
  ] },
  line:     { id: "line",     icon: "Li",     name: "LINE",                nameEn: "LINE",            hasQr: false, credFields: [
    { key: "channelSecret", label: "Channel Secret", type: "password", ph: "" },
    { key: "channelAccessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  zalo:     { id: "zalo",     icon: "Za",     name: "Zalo",                nameEn: "Zalo",            hasQr: false, credFields: [
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
    { key: "oaSecret", label: "OA Secret", type: "password", ph: "" },
  ] },
};

const FALLBACK_CH_ORDER_ZH = ["feishu","wechat","wecom","qq","dingtalk","telegram","matrix","discord","slack","whatsapp","signal","line","zalo"];
const FALLBACK_CH_ORDER_EN = ["telegram","matrix","discord","slack","whatsapp","signal","line","feishu","wechat","wecom","qq","dingtalk","zalo"];

// Search-engine <select> options previously inlined in rsclaw-panel.tsx.
const FALLBACK_SEARCH_ZH: CatalogSearchEngineDef[] = [
  { value: "bing-free",  label: "Bing (免费)" },
  { value: "baidu-free", label: "百度 (免费)" },
  { value: "sogou",      label: "搜狗 (免费)" },
  { value: "360",        label: "360搜索 (免费)" },
  { value: "duckduckgo", label: "DuckDuckGo (免费)" },
  { value: "google",     label: "Google (需 API Key)" },
  { value: "bing",       label: "Bing (需 API Key)" },
  { value: "brave",      label: "Brave (需 API Key)" },
  { value: "serper",     label: "Serper (需 API Key)" },
];
const FALLBACK_SEARCH_EN: CatalogSearchEngineDef[] = [
  { value: "bing-free",  label: "Bing (free)" },
  { value: "baidu-free", label: "Baidu (free)" },
  { value: "sogou",      label: "Sogou (free)" },
  { value: "360",        label: "360 Search (free)" },
  { value: "duckduckgo", label: "DuckDuckGo (free)" },
  { value: "google",     label: "Google (API key)" },
  { value: "bing",       label: "Bing (API key)" },
  { value: "brave",      label: "Brave (API key)" },
  { value: "serper",     label: "Serper (API key)" },
];

export const FALLBACK_CATALOG: Catalog = {
  providers: FALLBACK_PROVIDERS,
  providerOrderZh: FALLBACK_PROV_ORDER_ZH,
  providerOrderEn: FALLBACK_PROV_ORDER_EN,
  channels: FALLBACK_CHANNELS,
  channelOrderZh: FALLBACK_CH_ORDER_ZH,
  channelOrderEn: FALLBACK_CH_ORDER_EN,
  searchEnginesZh: FALLBACK_SEARCH_ZH,
  searchEnginesEn: FALLBACK_SEARCH_EN,
};

// ── Loading + cache ──────────────────────────────────────────────────────

let cachedPromise: Promise<Catalog> | null = null;
// Synchronously-available catalog: starts as FALLBACK, swapped to the live
// one once loaded so non-async accessors return real data after first load.
let resolvedCatalog: Catalog = FALLBACK_CATALOG;

async function fetchRawCatalog(): Promise<RawCatalog> {
  if (isTauriRuntime()) {
    // Tauri command returns a JSON STRING.
    const json = await invoke("read_defaults_catalog");
    if (typeof json === "string") return JSON.parse(json) as RawCatalog;
    // Defensive: some Tauri setups auto-parse — accept an object too.
    return json as RawCatalog;
  }
  const r = await gatewayFetch("/api/v1/defaults");
  if (!r.ok) throw new Error(`defaults fetch failed: ${r.status}`);
  return (await r.json()) as RawCatalog;
}

/**
 * Load + cache the catalog. Prefers Tauri (works before the gateway is up),
 * else gateway HTTP. On ANY failure returns FALLBACK_CATALOG. The result is
 * memoized in a module-level promise so concurrent callers share one fetch.
 */
export function loadCatalog(): Promise<Catalog> {
  if (cachedPromise) return cachedPromise;
  cachedPromise = (async () => {
    try {
      const raw = await fetchRawCatalog();
      const mapped = mapCatalog(raw);
      // Guard against an empty/partial backend payload — never blank the UI.
      if (Object.keys(mapped.providers).length === 0) {
        resolvedCatalog = FALLBACK_CATALOG;
        return FALLBACK_CATALOG;
      }
      resolvedCatalog = mapped;
      return mapped;
    } catch {
      resolvedCatalog = FALLBACK_CATALOG;
      return FALLBACK_CATALOG;
    }
  })();
  return cachedPromise;
}

/** Test helper / escape hatch: clear the module-level cache. */
export function resetCatalogCache(): void {
  cachedPromise = null;
  resolvedCatalog = FALLBACK_CATALOG;
}

// ── Synchronous accessors (operate on the latest resolved catalog) ───────

function isZh(lang?: string): boolean {
  return (lang || "") === "cn";
}

/** Provider lookup table (camelCase), keyed by id. Includes the grok alias. */
export function getProviderMap(): Record<string, CatalogProviderDef> {
  return resolvedCatalog.providers;
}

/** Provider ids in display order for the given language ("cn" → ZH order). */
export function getProviderOrder(lang?: string): string[] {
  return isZh(lang) ? resolvedCatalog.providerOrderZh : resolvedCatalog.providerOrderEn;
}

/** Channel lookup table (camelCase), keyed by id. */
export function getChannelMap(): Record<string, CatalogChannelDef> {
  return resolvedCatalog.channels;
}

/** Channel ids in display order for the given language ("cn" → ZH order). */
export function getChannelOrder(lang?: string): string[] {
  return isZh(lang) ? resolvedCatalog.channelOrderZh : resolvedCatalog.channelOrderEn;
}

/** Search-engine {value,label} options, sorted, localized to `lang`. */
export function getSearchEngines(lang?: string): CatalogSearchEngineDef[] {
  return isZh(lang) ? resolvedCatalog.searchEnginesZh : resolvedCatalog.searchEnginesEn;
}

// ── React hook ───────────────────────────────────────────────────────────

export interface UseCatalogResult {
  providers: Record<string, CatalogProviderDef>;
  channels: Record<string, CatalogChannelDef>;
  searchEnginesZh: CatalogSearchEngineDef[];
  searchEnginesEn: CatalogSearchEngineDef[];
  providerOrderZh: string[];
  providerOrderEn: string[];
  channelOrderZh: string[];
  channelOrderEn: string[];
  /** True once the async backend load has settled (success OR fallback). */
  loaded: boolean;
}

/**
 * React hook. Seeds from FALLBACK_CATALOG synchronously (first render is
 * always correct, never blank), then reconciles with the async backend load.
 * Zero-flash: the fallback IS the previous hardcoded data, so even if the
 * backend differs the swap is graceful.
 */
export function useCatalog(): UseCatalogResult {
  const [cat, setCat] = useState<Catalog>(resolvedCatalog);
  const [loaded, setLoaded] = useState<boolean>(cachedPromise != null);

  useEffect(() => {
    let alive = true;
    loadCatalog().then((c) => {
      if (!alive) return;
      setCat(c);
      setLoaded(true);
    });
    return () => {
      alive = false;
    };
  }, []);

  return {
    providers: cat.providers,
    channels: cat.channels,
    searchEnginesZh: cat.searchEnginesZh,
    searchEnginesEn: cat.searchEnginesEn,
    providerOrderZh: cat.providerOrderZh,
    providerOrderEn: cat.providerOrderEn,
    channelOrderZh: cat.channelOrderZh,
    channelOrderEn: cat.channelOrderEn,
    loaded,
  };
}
