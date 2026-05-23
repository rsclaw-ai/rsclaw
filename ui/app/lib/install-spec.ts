export type PluginRuntime = "wasm" | "js";

export interface HubPluginCatalogEntry {
  slug: string;
  version: string;
  installed: boolean;
  description: string;
  runtime?: PluginRuntime;
}

export function normalizeInstallSpec(spec: string): string {
  const trimmed = spec.trim();
  if (trimmed.startsWith("@/")) return trimmed.slice(1);
  return trimmed;
}

export function recommendedPluginsForRuntime(
  plugins: HubPluginCatalogEntry[],
  runtime: PluginRuntime,
): HubPluginCatalogEntry[] {
  return plugins.filter((plugin) => {
    if (plugin.installed) return false;
    if (!plugin.runtime) return true;
    return plugin.runtime === runtime;
  });
}
