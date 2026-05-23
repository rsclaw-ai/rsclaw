import fs from "fs";
import path from "path";

describe("computer-use overlay styles", () => {
  test("uses palette variables instead of bare color literals in rules", () => {
    const css = fs.readFileSync(
      path.join(process.cwd(), "app/components/computer-use-overlay.module.scss"),
      "utf8",
    );
    const ruleBody = css
      .split("\n")
      .filter((line) => !line.trimStart().startsWith("$"))
      .join("\n");

    expect(ruleBody).not.toMatch(/#[0-9a-fA-F]{3,8}\b|rgba?\(/);
  });
});
