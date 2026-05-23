import { parseScreenshotMessage } from "../app/lib/message-attachments";

describe("parseScreenshotMessage", () => {
  test("recognizes desktop screenshot JSON messages", () => {
    const parsed = parseScreenshotMessage(`{
      "action": "screenshot",
      "image_path": "/Users/oopos/Downloads/rsclaw/screenshots/18b220751064ef40.jpg",
      "mime": "image/jpeg",
      "width": 1024,
      "height": 640,
      "original_width": 2880,
      "original_height": 1800,
      "scale": 2.8125
    }`);

    expect(parsed).toEqual({
      action: "screenshot",
      imagePath: "/Users/oopos/Downloads/rsclaw/screenshots/18b220751064ef40.jpg",
      mime: "image/jpeg",
      width: 1024,
      height: 640,
      originalWidth: 2880,
      originalHeight: 1800,
      scale: 2.8125,
    });
  });

  test("ignores non-image JSON messages", () => {
    expect(parseScreenshotMessage('{"action":"log","image_path":"/tmp/a.jpg","mime":"image/jpeg"}')).toBeNull();
    expect(parseScreenshotMessage('{"action":"screenshot","image_path":"/tmp/a.txt","mime":"text/plain"}')).toBeNull();
  });
});
