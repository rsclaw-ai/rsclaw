let GATEWAY_URL =
  process.env.NEXT_PUBLIC_RSCLAW_GATEWAY_URL || "http://localhost:18888";
let AUTH_TOKEN = process.env.NEXT_PUBLIC_RSCLAW_AUTH_TOKEN || "";

// Allow runtime update of gateway URL (e.g. from Tauri config read)
export function setGatewayUrl(url: string) {
  GATEWAY_URL = url;
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
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...(AUTH_TOKEN ? { Authorization: `Bearer ${AUTH_TOKEN}` } : {}),
  };
  return fetch(`${GATEWAY_URL}${path}`, {
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

export async function reloadConfig() {
  return gatewayFetch("/api/v1/config/reload", { method: "POST" }).then((r) =>
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
  return gatewayFetch(`/api/v1/memory/docs${qs ? "?" + qs : ""}`, {
    signal: AbortSignal.timeout(15000),
  }).then((r) => r.json());
}

export async function getMemoryStats(): Promise<MemoryStatsResponse> {
  return gatewayFetch("/api/v1/memory/stats", {
    signal: AbortSignal.timeout(8000),
  }).then((r) => r.json());
}

export { GATEWAY_URL, AUTH_TOKEN };
