import pkg from "../package.json";

describe("Tauri dev scripts", () => {
  test("export:dev runs Next dev without static export mode", () => {
    expect(pkg.scripts["export:dev"]).toContain("next dev");
    expect(pkg.scripts["export:dev"]).not.toContain("BUILD_MODE=export");
  });
});
