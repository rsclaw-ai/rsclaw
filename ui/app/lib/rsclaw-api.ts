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

export async function testProviderKey(provider: string, apiKey: string, baseUrl?: string) {
  return gatewayFetch("/api/v1/providers/test", {
    method: "POST",
    body: JSON.stringify({ provider, api_key: apiKey, base_url: baseUrl }),
    signal: AbortSignal.timeout(20000),
  }).then((r) => r.json());
}

export async function listProviderModels(provider: string, apiKey: string, baseUrl?: string) {
  return gatewayFetch("/api/v1/providers/models", {
    method: "POST",
    body: JSON.stringify({ provider, api_key: apiKey, base_url: baseUrl }),
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

export { GATEWAY_URL, AUTH_TOKEN };
