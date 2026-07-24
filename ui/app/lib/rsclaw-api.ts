let GATEWAY_URL =
  process.env.NEXT_PUBLIC_RSCLAW_GATEWAY_URL || "http://localhost:18888";
let AUTH_TOKEN = process.env.NEXT_PUBLIC_RSCLAW_AUTH_TOKEN || "";

// Allow runtime update of gateway URL (e.g. from Tauri config read)
export function setGatewayUrl(url: string) {
  GATEWAY_URL = url;
  // Persist so getGatewayUrl()'s localStorage fallback returns this fresh value
  // rather than a stale cache (e.g. an old debug port the user has since changed
  // back). Desktop startup calls this with the Tauri-resolved gateway port, so
  // the cache re-syncs to the real port on every launch.
  try { localStorage.setItem("rsclaw-gateway-url", url); } catch {}
}
export function getGatewayUrl() {
  if (GATEWAY_URL && GATEWAY_URL !== "http://localhost:18888") return GATEWAY_URL;
  try { return localStorage.getItem("rsclaw-gateway-url") || GATEWAY_URL; } catch {}
  return GATEWAY_URL;
}
export function setAuthToken(token: string) {
  AUTH_TOKEN = token;
}
export function getAuthToken() {
  if (AUTH_TOKEN) return AUTH_TOKEN;
  try { return localStorage.getItem("rsclaw-auth-token") || ""; } catch {}
  return "";
}

export async function gatewayFetch(
  path: string,
  options?: RequestInit,
): Promise<Response> {
  // Resolve URL + token through the accessors so localStorage fallbacks
  // and Tauri-set runtime values always win over module-init env defaults.
  // The closure-cached `GATEWAY_URL` / `AUTH_TOKEN` were the source of a
  // "stuck offline" bug: env (.env.local) pinned URL to a stale port and
  // requests kept hitting it even after Tauri called setGatewayUrl().
  const url = getGatewayUrl();
  const token = getAuthToken();
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
  };
  return fetch(`${url}${path}`, {
    ...options,
    headers: { ...headers, ...(options?.headers as Record<string, string>) },
  });
}

export async function getHealth() {
  return gatewayFetch("/api/v1/health", {
    signal: AbortSignal.timeout(3000),
  }).then((r) => r.json());
}

export async function getStatus() {
  return gatewayFetch("/api/v1/status", {
    signal: AbortSignal.timeout(3000),
  }).then((r) => r.json());
}

export async function getConfig() {
  return gatewayFetch("/api/v1/config").then((r) => r.json());
}

export async function saveConfig(config: any) {
  return gatewayFetch("/api/v1/config", {
    method: "PUT",
    body: JSON.stringify(config),
  }).then((r) => r.json());
}

export async function reloadConfig(scope?: string[]) {
  const query = scope?.length
    ? `?scope=${encodeURIComponent(scope.join(","))}`
    : "";
  return gatewayFetch(`/api/v1/reload${query}`, { method: "POST" }).then(async (r) => {
    if (!r.ok) throw new Error(await r.text());
    return r.json();
  });
}

/**
 * Graceful re-exec of the gateway binary. Backend does flag-and-flush:
 * sets the shutdown flag, returns `{ restarting: true }` immediately,
 * then axum's `with_graceful_shutdown` drains in-flight requests before
 * the listener releases the port and a replacement process binds.
 *
 * Loopback-only — backend rejects with 403 from non-127.0.0.1 peers.
 * Use after operations that change live state the gateway caches in-
 * memory (e.g. installing a plugin: its WASM/manifest needs to be
 * registered into PluginRegistry, which only happens at boot).
 */
export async function restartGateway() {
  return gatewayFetch("/api/v1/restart", { method: "POST" }).then((r) =>
    r.json(),
  );
}

export async function getLogs(limit: number = 50) {
  return gatewayFetch(`/api/v1/logs?limit=${limit}`, {
    signal: AbortSignal.timeout(3000),
  }).then((r) => r.json());
}

export async function getAgents() {
  return gatewayFetch("/api/v1/agents", {
    signal: AbortSignal.timeout(3000),
  }).then((r) => r.json());
}

export async function saveAgent(agent: any) {
  return gatewayFetch("/api/v1/agents", {
    method: "POST",
    body: JSON.stringify(agent),
  }).then((r) => r.json());
}

export async function deleteAgent(id: string) {
  return gatewayFetch(`/api/v1/agents/${encodeURIComponent(id)}`, {
    method: "DELETE",
  }).then((r) => r.json());
}

export async function clearSession(sessionKey: string) {
  return gatewayFetch(
    `/api/v1/sessions/${encodeURIComponent(sessionKey)}/clear`,
    { method: "POST" },
  ).then((r) => r.json());
}

export async function testProviderKey(provider: string, apiKey: string, baseUrl?: string, apiType?: string) {
  return gatewayFetch("/api/v1/providers/test", {
    method: "POST",
    body: JSON.stringify({ provider, api_key: apiKey, base_url: baseUrl, api_type: apiType }),
    signal: AbortSignal.timeout(20000),
  }).then((r) => r.json());
}

export async function listProviderModels(provider: string, apiKey: string, baseUrl?: string, apiType?: string) {
  return gatewayFetch("/api/v1/providers/models", {
    method: "POST",
    body: JSON.stringify({ provider, api_key: apiKey, base_url: baseUrl, api_type: apiType }),
    signal: AbortSignal.timeout(20000),
  }).then((r) => r.json());
}

export async function wechatQrStart() {
  return gatewayFetch("/api/v1/channels/wechat/qr-login", {
    method: "POST",
    signal: AbortSignal.timeout(10000),
  }).then((r) => r.json());
}

export async function wechatQrStatus(qrcodeToken: string) {
  return gatewayFetch("/api/v1/channels/wechat/qr-status", {
    method: "POST",
    body: JSON.stringify({ qrcode_token: qrcodeToken }),
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function runDoctor() {
  return gatewayFetch("/api/v1/doctor", {
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function runDoctorFix() {
  return gatewayFetch("/api/v1/doctor/fix", {
    method: "POST",
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function listWorkspaceFiles(agentId?: string) {
  const q = agentId ? `?agent=${encodeURIComponent(agentId)}` : "";
  return gatewayFetch(`/api/v1/workspace/files${q}`, {
    signal: AbortSignal.timeout(5000),
  }).then((r) => r.json());
}

export async function readWorkspaceFile(fileName: string, agentId?: string) {
  const q = agentId ? `?agent=${encodeURIComponent(agentId)}` : "";
  return gatewayFetch(
    `/api/v1/workspace/files/${encodeURIComponent(fileName)}${q}`,
    { signal: AbortSignal.timeout(5000) },
  ).then((r) => r.json());
}

export async function writeWorkspaceFile(
  fileName: string,
  content: string,
  agentId?: string,
) {
  const q = agentId ? `?agent=${encodeURIComponent(agentId)}` : "";
  return gatewayFetch(
    `/api/v1/workspace/files/${encodeURIComponent(fileName)}${q}`,
    { method: "PUT", body: JSON.stringify({ content }) },
  ).then((r) => r.json());
}

// ---------------------------------------------------------------------------
// Memory management (read-only browse for the desktop UI)
// ---------------------------------------------------------------------------

export type MemoryDoc = {
  id: string;
  scope: string;
  kind: string;
  text: string;
  abstract_text: string | null;
  overview_text: string | null;
  tags: string[];
  tier: "core" | "working" | "peripheral";
  importance: number;
  pinned: boolean;
  created_at: number;
  accessed_at: number;
  access_count: number;
  /** Computed server-side via Weibull stretched-exponential decay. */
  relevance_score: number;
};

export type MemoryListResponse = {
  docs: MemoryDoc[];
  /** Total before `limit` was applied. */
  total: number;
};

export type MemoryStatsResponse = {
  total: number;
  by_tier: Record<string, number>;
  by_kind: Record<string, number>;
  by_scope: Record<string, number>;
  pinned: number;
};

export type MemoryListFilters = {
  /** Semantic-search query. Empty / undefined → list all. */
  q?: string;
  scope?: string;
  kind?: string;
  /** Default 200, hard cap 1000 server-side. */
  limit?: number;
};

export async function listMemoryDocs(
  filters?: MemoryListFilters,
): Promise<MemoryListResponse> {
  const params = new URLSearchParams();
  if (filters?.q) params.set("q", filters.q);
  if (filters?.scope) params.set("scope", filters.scope);
  if (filters?.kind) params.set("kind", filters.kind);
  if (filters?.limit) params.set("limit", String(filters.limit));
  const qs = params.toString();
  const r = await gatewayFetch(`/api/v1/memory/docs${qs ? "?" + qs : ""}`, {
    signal: AbortSignal.timeout(15000),
  });
  // Reject non-2xx so an error envelope (401/500 body) never gets parsed
  // as a MemoryListResponse — the caller's catch handles it.
  if (!r.ok) throw new Error(`memory/docs ${r.status}`);
  return r.json();
}

export async function getMemoryStats(): Promise<MemoryStatsResponse> {
  const r = await gatewayFetch("/api/v1/memory/stats", {
    signal: AbortSignal.timeout(8000),
  });
  // Same guard — without it a 401 body like {"error":"..."} would be
  // assigned to `stats` and Object.keys(stats.by_kind) crashes the page.
  if (!r.ok) throw new Error(`memory/stats ${r.status}`);
  return r.json();
}

// ---------------------------------------------------------------------------
// Hub catalog (read-only) — tools / skills / plugins for the desktop module.
// Backed by GET /api/v1/hub/{catalog,tools,skills,plugins}. `installed` reflects
// local state; descriptions/versions come from the signed hub manifest.
// ---------------------------------------------------------------------------

export interface HubToolEntry {
  name: string;
  description: string;
  version: string;
  installed: boolean;
  installed_version: string | null;
}

export interface HubSkillEntry {
  slug: string;
  version: string;
  installed: boolean;
  publisher: string;
  description: string;
}

export interface HubPluginEntry {
  slug: string;
  version: string;
  installed: boolean;
  description: string;
}

export interface HubCatalog {
  tools: HubToolEntry[];
  skills: HubSkillEntry[];
  plugins: HubPluginEntry[];
}

export async function getHubCatalog(): Promise<HubCatalog> {
  // May lazy-fetch the hub manifests on a cold gateway → generous timeout.
  return gatewayFetch("/api/v1/hub/catalog", {
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function getHubTools(): Promise<HubToolEntry[]> {
  return gatewayFetch("/api/v1/hub/tools", {
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function getHubSkills(): Promise<HubSkillEntry[]> {
  return gatewayFetch("/api/v1/hub/skills", {
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

export async function getHubPlugins(): Promise<HubPluginEntry[]> {
  return gatewayFetch("/api/v1/hub/plugins", {
    signal: AbortSignal.timeout(30000),
  }).then((r) => r.json());
}

// ---------------------------------------------------------------------------
// Model health (chain-failover state)
// ---------------------------------------------------------------------------
// Surface the per-model status the gateway's FailoverManager tracks so the
// model-config UI can show ● green / ● yellow / ● red dots next to every
// entry in a chain. Lazy-populated server-side: a model id only shows up in
// the snapshot after the runtime has tried to call it at least once. Models
// configured but never invoked are absent from `models[]` — UI defaults the
// dot to gray for those.

export interface ModelHealthEntry {
  model: string;
  /** "Healthy" | "Cooling" | "Disabled" — exact strings the dot maps from. */
  status: string;
  /** Disabled only. Examples: "Balance" / "Auth" / "ModelMissing" /
   *  "RateLimit" / "Transient" / "Unknown". Null otherwise. */
  reason: string | null;
  /** Cooling only — seconds until the entry becomes callable again. */
  cooldown_seconds: number | null;
  /** Last failure body (≤200 chars), for the dot's tooltip. May be null. */
  last_error: string | null;
  consecutive_failures: number;
}

export async function getModelHealth(): Promise<{ models: ModelHealthEntry[] }> {
  return gatewayFetch("/api/v1/models/health", {
    signal: AbortSignal.timeout(5000),
  }).then((r) => r.json());
}

export async function resetModelHealth(
  model: string,
): Promise<{ ok: boolean; reset?: string; error?: string }> {
  return gatewayFetch("/api/v1/models/health/reset", {
    method: "POST",
    body: JSON.stringify({ model }),
  }).then((r) => r.json());
}

export { GATEWAY_URL, AUTH_TOKEN };
