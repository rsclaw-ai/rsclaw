import { normalizeInstallSpec, recommendedPluginsForRuntime } from "../app/lib/install-spec";

describe("normalizeInstallSpec", () => {
  test("removes chat-style @ prefix from absolute macOS paths", () => {
    expect(normalizeInstallSpec("@/Users/oopos/Downloads/travel.zip")).toBe(
      "/Users/oopos/Downloads/travel.zip",
    );
  });

  test("keeps scoped package and URL specs unchanged", () => {
    expect(normalizeInstallSpec("@scope/plugin")).toBe("@scope/plugin");
    expect(normalizeInstallSpec("https://example.com/plugin.zip")).toBe("https://example.com/plugin.zip");
  });
});

describe("recommendedPluginsForRuntime", () => {
  test("hides installed plugins from recommendations", () => {
    expect(
      recommendedPluginsForRuntime(
        [
          { slug: "travel", version: "0.1.0", installed: true, description: "Travel" },
          { slug: "other", version: "0.1.0", installed: false, description: "Other" },
        ],
        "wasm",
      ).map((plugin) => plugin.slug),
    ).toEqual(["other"]);
  });

  test("keeps runtime-specific recommendations in their matching tab", () => {
    const plugins = [
      { slug: "travel", version: "0.1.0", installed: false, description: "Travel", runtime: "js" as const },
      { slug: "sandbox", version: "0.1.0", installed: false, description: "Sandbox", runtime: "wasm" as const },
    ];

    expect(recommendedPluginsForRuntime(plugins, "wasm").map((plugin) => plugin.slug)).toEqual(["sandbox"]);
    expect(recommendedPluginsForRuntime(plugins, "js").map((plugin) => plugin.slug)).toEqual(["travel"]);
  });
});
