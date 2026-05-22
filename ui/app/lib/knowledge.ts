/**
 * Typed wrappers around `/api/v1/knowledge/*`.
 *
 * Wire contract documented in `docs/interfaces/knowledge-api.md`. Routes
 * 404 when the KB store failed to open at gateway startup — callers
 * should treat any 404 on `/stats` as "KB disabled" and surface that.
 *
 * Multipart upload bypasses `gatewayFetch` because that helper hard-sets
 * `Content-Type: application/json`. We hit `fetch` directly and let the
 * browser pick the boundary for FormData. Auth header still added by hand.
 *
 * SSE uses `@fortaine/fetch-event-source` so we can attach the Bearer
 * token (native EventSource cannot set custom headers).
 */
import JSON5 from "json5";
import { fetchEventSource } from "@fortaine/fetch-event-source";

import { gatewayFetch, getAuthToken, getGatewayUrl } from "./rsclaw-api";

const DEFAULT_MAX_DOC_BYTES = 50 * 1024 * 1024;

// ── Types ──────────────────────────────────────────────────────────

export interface KbCollection {
  id: string;
  name: string;
  description: string | null;
  embedModel: string | null;
  embedDim: number;
  docCount: number;
  chunkCount: number;
  bytes: number;
  createdAt: string;
  updatedAt: string;
}

export type KbDocStatus = "pending" | "indexing" | "ready" | "failed";

export interface KbDoc {
  id: string;
  title: string;
  source: string;
  mime: string;
  bytes: number;
  chunkCount: number;
  status: KbDocStatus;
  indexedAt: string | null;
  createdAt: string;
}

/**
 * Server's 202 envelope for `POST …/docs` (JSON or multipart). Carries
 * only enough to identify the doc — `mime/chunkCount/indexedAt/createdAt`
 * land later via SSE / refetch. Don't treat this as a `KbDoc`.
 */
export interface KbUploadAccepted {
  id: string;
  title: string;
  status: string;
  bytes: number;
}

/**
 * Response shape for `POST …/docs/from-url`. UrlSyncer's job-level
 * envelope — no per-doc id is returned (one URL may dedupe to an existing
 * doc, hence `docsSkipped`). Use SSE / listDocs() to surface the result.
 */
export interface KbUrlIngestAccepted {
  status: "pending" | "skipped";
  docsAdded: number;
  docsSkipped: number;
}

export interface KbSearchHit {
  docId: string;
  collectionId: string | null;
  collectionName: string | null;
  sourceTitle: string;
  chunkText: string;
  score: number;
}

export interface KbSearchResult {
  hits: KbSearchHit[];
  queryMs: number;
}

export interface KbStats {
  collectionCount: number;
  docCount: number;
  chunkCount: number;
  bytes: number;
}

export interface KbEmbedder {
  id: string;
  label: string;
  dim: number;
  downloaded: boolean;
}

export interface KbEmbedders {
  default: string | null;
  available: KbEmbedder[];
}

// ── Helpers ───────────────────────────────────────────────────────

export class KbDisabledError extends Error {
  constructor() {
    super("KB store not available (gateway returned 404)");
    this.name = "KbDisabledError";
  }
}

async function ok<T>(res: Response): Promise<T> {
  if (res.status === 404) {
    // The 404 envelope `{ error: "collection_not_found" }` is meaningful
    // for sub-resources; bare `/stats` 404 = whole KB disabled. Disambiguate
    // by inspecting the path or body. Caller can also catch KbDisabledError.
    let body: any = null;
    try {
      body = await res.json();
    } catch {
      /* ignore */
    }
    if (!body?.error) throw new KbDisabledError();
    const err: any = new Error(body.error);
    err.code = body.error;
    err.status = 404;
    throw err;
  }
  if (!res.ok) {
    let body: any = null;
    try {
      body = await res.json();
    } catch {
      /* ignore */
    }
    const err: any = new Error(body?.error || res.statusText || `http ${res.status}`);
    err.code = body?.error;
    err.status = res.status;
    throw err;
  }
  return res.json() as Promise<T>;
}

// ── Collections ───────────────────────────────────────────────────

export async function listCollections(): Promise<KbCollection[]> {
  const res = await gatewayFetch("/api/v1/knowledge/collections");
  return (await ok<{ collections: KbCollection[] }>(res)).collections;
}

export async function createCollection(input: {
  name: string;
  description?: string;
  embedModel?: string;
}): Promise<KbCollection> {
  const res = await gatewayFetch("/api/v1/knowledge/collections", {
    method: "POST",
    body: JSON.stringify(input),
  });
  return ok<KbCollection>(res);
}

export async function getCollection(id: string): Promise<KbCollection> {
  const res = await gatewayFetch(`/api/v1/knowledge/collections/${encodeURIComponent(id)}`);
  return ok<KbCollection>(res);
}

export async function patchCollection(
  id: string,
  patch: { name?: string; description?: string },
): Promise<KbCollection> {
  const res = await gatewayFetch(`/api/v1/knowledge/collections/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
  return ok<KbCollection>(res);
}

export async function deleteCollection(id: string): Promise<{ deletedDocs: number }> {
  const res = await gatewayFetch(`/api/v1/knowledge/collections/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
  return ok<{ deletedDocs: number }>(res);
}

// ── Docs ──────────────────────────────────────────────────────────

export async function listDocs(collectionId: string): Promise<KbDoc[]> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs`,
  );
  return (await ok<{ docs: KbDoc[]; nextCursor: string | null }>(res)).docs;
}

export async function getDoc(collectionId: string, docId: string): Promise<KbDoc> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/${encodeURIComponent(docId)}`,
  );
  return ok<KbDoc>(res);
}

export async function getDocContent(collectionId: string, docId: string): Promise<string> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/${encodeURIComponent(docId)}/content`,
  );
  if (!res.ok) throw new Error(`http ${res.status}`);
  return res.text();
}

/**
 * Upload via JSON. Server returns 202 with a partial envelope (id/title/
 * status/bytes) — NOT a full KbDoc. mime/chunkCount/indexedAt/createdAt
 * fill in later via SSE or a follow-up listDocs() call. Treat this as
 * fire-and-refresh: don't drop the return value into a KbDoc-shaped slot.
 */
export async function uploadDocJson(
  collectionId: string,
  input: { title: string; text: string; mime?: string },
): Promise<KbUploadAccepted> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs`,
    {
      method: "POST",
      body: JSON.stringify(input),
    },
  );
  return ok<KbUploadAccepted>(res);
}

/**
 * Multipart upload. Same 202 / partial-envelope semantics as uploadDocJson;
 * see KbUploadAccepted. We DON'T go through gatewayFetch because that
 * helper always sets `Content-Type: application/json` — multipart needs
 * the browser to set the Content-Type with its own random boundary.
 */
export async function uploadDocFile(
  collectionId: string,
  file: File,
  title?: string,
): Promise<KbUploadAccepted> {
  const fd = new FormData();
  if (title) fd.append("title", title);
  fd.append("file", file);
  const token = getAuthToken();
  const res = await fetch(
    `${getGatewayUrl()}/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs`,
    {
      method: "POST",
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
      body: fd,
    },
  );
  return ok<KbUploadAccepted>(res);
}

/**
 * Hand a URL to the backend's UrlSyncer. Same async path as multipart/JSON
 * uploads — returns 202 immediately and the doc indexes in the background.
 * Backend derives the title from the URL/page; no title field accepted.
 *
 * Caveat: this is a JOB envelope, NOT a doc handle — there's no `id`,
 * because a single URL may dedupe to an existing doc (hence docsSkipped).
 * Callers MUST refresh via listDocs()/SSE to learn the resulting doc.
 */
export async function uploadDocFromUrl(
  collectionId: string,
  url: string,
): Promise<KbUrlIngestAccepted> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/from-url`,
    {
      method: "POST",
      body: JSON.stringify({ url }),
    },
  );
  return ok<KbUrlIngestAccepted>(res);
}

/**
 * True when the gateway runs on this same machine (desktop app talking to a
 * loopback gateway). Only then is the `from-path` optimization valid — the
 * gateway must be able to std::fs::read the path the client hands it. A remote
 * UI (web, or pointed at a LAN gateway) must keep uploading bytes.
 */
export function isSameMachineGateway(): boolean {
  const isTauri =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  if (!isTauri) return false;
  try {
    const host = new URL(getGatewayUrl()).hostname.toLowerCase();
    return host === "127.0.0.1" || host === "localhost" || host === "::1";
  } catch {
    return false;
  }
}

/**
 * Same-machine fast path: hand the gateway an absolute local path and let it
 * read the file itself, skipping the read-into-JS → multipart → write-to-disk
 * byte round-trip. Backend is loopback-gated and path-allowlisted; callers
 * MUST first confirm isSameMachineGateway(). Same 202 envelope as the others.
 */
export async function uploadDocFromPath(
  collectionId: string,
  path: string,
): Promise<KbUploadAccepted> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/from-path`,
    {
      method: "POST",
      body: JSON.stringify({ path }),
    },
  );
  return ok<KbUploadAccepted>(res);
}

export interface KbDirIngestAccepted {
  status: string;
  docsAdded: number;
  docsSkipped: number;
  /** True when the directory had more files than the per-call cap. */
  truncated: boolean;
}

/**
 * Same-machine recursive directory import: the gateway walks the tree and
 * ingests every supported file. Loopback-gated + path-allowlisted like
 * from-path; callers MUST first confirm isSameMachineGateway(). Returns a
 * summary (added/skipped/truncated), not a single doc handle.
 */
export async function uploadDocFromDir(
  collectionId: string,
  path: string,
): Promise<KbDirIngestAccepted> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/from-dir`,
    {
      method: "POST",
      body: JSON.stringify({ path }),
    },
  );
  return ok<KbDirIngestAccepted>(res);
}

export async function deleteDoc(
  collectionId: string,
  docId: string,
): Promise<{ deleted: boolean }> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/${encodeURIComponent(docId)}`,
    { method: "DELETE" },
  );
  return ok<{ deleted: boolean }>(res);
}

export async function reindexDoc(
  collectionId: string,
  docId: string,
): Promise<{ status: string }> {
  const res = await gatewayFetch(
    `/api/v1/knowledge/collections/${encodeURIComponent(collectionId)}/docs/${encodeURIComponent(docId)}/reindex`,
    { method: "POST" },
  );
  return ok<{ status: string }>(res);
}

// ── Search / stats / embedders ────────────────────────────────────

export async function search(input: {
  query: string;
  collectionIds?: string[];
  topK?: number;
  scoreThreshold?: number;
}): Promise<KbSearchResult> {
  const res = await gatewayFetch("/api/v1/knowledge/search", {
    method: "POST",
    body: JSON.stringify(input),
  });
  return ok<KbSearchResult>(res);
}

export async function getStats(): Promise<KbStats> {
  const res = await gatewayFetch("/api/v1/knowledge/stats");
  return ok<KbStats>(res);
}

export async function getEmbedders(): Promise<KbEmbedders> {
  const res = await gatewayFetch("/api/v1/knowledge/embedders");
  return ok<KbEmbedders>(res);
}

// ── Limits (resolved from gateway config) ────────────────────────

/**
 * Resolve `kb.maxDocMb` from `/api/v1/config` (returns `{ raw, path }`,
 * `raw` being the JSON5 source of rsclaw.json5). Cached for the session —
 * if the user reloads the gateway config we'd want to re-fetch, but
 * config edits are infrequent enough that one-shot is fine in v1.
 * Falls back to 50 MB if anything fails (matches backend default).
 */
let cachedMaxDocBytes: number | null = null;
export async function getMaxDocBytes(): Promise<number> {
  if (cachedMaxDocBytes !== null) return cachedMaxDocBytes;
  try {
    const res = await gatewayFetch("/api/v1/config");
    if (!res.ok) {
      cachedMaxDocBytes = DEFAULT_MAX_DOC_BYTES;
      return cachedMaxDocBytes;
    }
    const body = (await res.json()) as { raw?: string };
    const cfg = JSON5.parse(body.raw || "{}") as any;
    const mb = cfg?.kb?.maxDocMb;
    cachedMaxDocBytes =
      typeof mb === "number" && mb > 0 ? mb * 1024 * 1024 : DEFAULT_MAX_DOC_BYTES;
    return cachedMaxDocBytes;
  } catch {
    cachedMaxDocBytes = DEFAULT_MAX_DOC_BYTES;
    return cachedMaxDocBytes;
  }
}

// ── SSE doc status stream ─────────────────────────────────────────

export interface KbDocStatusEvent {
  type: "knowledge.doc.status_changed";
  docId: string;
  status: KbDocStatus;
}

/**
 * Subscribe to `/api/v1/knowledge/events`. Returns an unsubscribe fn.
 * `onEvent` fires on every doc-status change; UI typically uses it to
 * patch the affected card without a full refetch.
 *
 * Reconnect is handled by fetchEventSource automatically — we just
 * surface terminal errors so the caller can show "stream disconnected".
 */
export function subscribeDocStatus(
  onEvent: (ev: KbDocStatusEvent) => void,
  onError?: (e: unknown) => void,
): () => void {
  const ctrl = new AbortController();
  const token = getAuthToken();
  void fetchEventSource(`${getGatewayUrl()}/api/v1/knowledge/events`, {
    signal: ctrl.signal,
    headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    openWhenHidden: true,
    onmessage(ev) {
      if (!ev.data) return;
      try {
        const payload = JSON.parse(ev.data) as KbDocStatusEvent;
        if (payload.type === "knowledge.doc.status_changed") onEvent(payload);
      } catch {
        /* ignore non-JSON keep-alives */
      }
    },
    onerror(err) {
      onError?.(err);
      // Return number = retry delay; throwing aborts. We let fetchEventSource
      // reconnect by returning undefined (it picks its default backoff).
    },
  }).catch((e) => onError?.(e));
  return () => ctrl.abort();
}
