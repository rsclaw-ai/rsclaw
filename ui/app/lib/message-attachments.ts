export interface ScreenshotMessage {
  action: "screenshot";
  imagePath: string;
  mime: string;
  width?: number;
  height?: number;
  originalWidth?: number;
  originalHeight?: number;
  scale?: number;
}

export function parseScreenshotMessage(content: string): ScreenshotMessage | null {
  const trimmed = content.trim();
  if (!trimmed.startsWith("{")) return null;
  let data: unknown;
  try {
    data = JSON.parse(trimmed);
  } catch {
    return null;
  }

  if (!data || typeof data !== "object") return null;
  const record = data as Record<string, unknown>;
  if (record.action !== "screenshot") return null;
  if (typeof record.image_path !== "string" || record.image_path.length === 0) return null;
  const mime = typeof record.mime === "string" ? record.mime : "image/jpeg";
  if (!mime.startsWith("image/")) return null;

  return {
    action: "screenshot",
    imagePath: record.image_path,
    mime,
    width: typeof record.width === "number" ? record.width : undefined,
    height: typeof record.height === "number" ? record.height : undefined,
    originalWidth: typeof record.original_width === "number" ? record.original_width : undefined,
    originalHeight: typeof record.original_height === "number" ? record.original_height : undefined,
    scale: typeof record.scale === "number" ? record.scale : undefined,
  };
}
