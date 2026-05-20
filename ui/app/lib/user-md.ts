/**
 * Helpers for the per-agent `USER.md` workspace file.
 *
 * Surface area is small on purpose — we only need to (a) read the
 * current agent's USER.md and (b) decide whether it's still the
 * empty placeholder seeded by `rsclaw setup`. The actual writing
 * is handled by the agent itself: when prompted, it uses the
 * `ask_user` tool to collect a few preferences and then
 * `write_workspace_file` to persist the markdown. The desktop UI
 * just nudges that flow and watches for the result.
 */

import { invoke, isTauri } from "../utils/tauri";

const USER_MD_FILE = "USER.md";

/**
 * Read the USER.md content for the given agent. Returns an empty
 * string if the file doesn't exist or anything went wrong — both
 * cases get treated the same downstream (`isUserMdDefault` returns
 * true), so the banner appears for fresh installs.
 */
export async function readUserMd(agentId: string): Promise<string> {
  if (!isTauri || !agentId) return "";
  try {
    const content = (await invoke("read_workspace_file", {
      agentId,
      fileName: USER_MD_FILE,
    })) as string;
    return content || "";
  } catch {
    return "";
  }
}

/**
 * Heuristic: strip HTML comments + markdown headings + whitespace
 * and check whether anything's left. The seeded placeholder is just
 * a `# USER.md` heading plus a couple of `<!-- ... -->` comments,
 * so it collapses to empty. Any prose the user (or the agent) wrote
 * survives the strip and trips the "customised" branch.
 */
export function isUserMdDefault(content: string): boolean {
  if (!content) return true;
  const stripped = content
    .replace(/<!--[\s\S]*?-->/g, "")
    .replace(/^#.*$/gm, "")
    .trim();
  return stripped.length === 0;
}
