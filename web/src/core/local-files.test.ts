import { describe, expect, test } from "bun:test";
import { fileReference, imageSource, isMarkdown, viewerTarget, viewerUrl } from "./local-files";

describe("file references", () => {
  test("absolute, relative, and file:// paths name local files", () => {
    expect(fileReference("/Users/me/scratch/report.md")).toEqual({ path: "/Users/me/scratch/report.md", line: null, fragment: "" });
    expect(fileReference("docs/web.md")).toEqual({ path: "docs/web.md", line: null, fragment: "" });
    expect(fileReference("../notes.md")).toEqual({ path: "../notes.md", line: null, fragment: "" });
    expect(fileReference("file:///tmp/a.txt")?.path).toBe("/tmp/a.txt");
    expect(fileReference("file://localhost/tmp/a.txt")?.path).toBe("/tmp/a.txt");
  });

  test("percent-encoding is decoded and a malformed escape is kept as written", () => {
    expect(fileReference("/Users/me/My%20Docs/r%C3%A9sum%C3%A9.md")?.path).toBe("/Users/me/My Docs/résumé.md");
    expect(fileReference("/Users/me/My Docs/report.md")?.path).toBe("/Users/me/My Docs/report.md");
    expect(fileReference("/tmp/100%.txt")?.path).toBe("/tmp/100%.txt");
  });

  test("an editor position suffix becomes the line and a fragment is kept apart", () => {
    expect(fileReference("report.md:12")).toEqual({ path: "report.md", line: 12, fragment: "" });
    expect(fileReference("report.md:12:4#summary")).toEqual({ path: "report.md", line: 12, fragment: "summary" });
    expect(fileReference("src/app.ts:42")).toEqual({ path: "src/app.ts", line: 42, fragment: "" });
    expect(fileReference("/abs/lib.rs:40:7")).toEqual({ path: "/abs/lib.rs", line: 40, fragment: "" });
    expect(fileReference("docs/web.md#remote-access")).toEqual({ path: "docs/web.md", line: null, fragment: "remote-access" });
    expect(fileReference("docs/a%23b.md")?.path).toBe("docs/a#b.md");
    expect(fileReference("/tmp/report.md%3A12")).toEqual({ path: "/tmp/report.md:12", line: null, fragment: "" });
    expect(fileReference("report.md%3A12:4")).toEqual({ path: "report.md:12", line: 4, fragment: "" });
  });

  test("remote URLs and bare fragments are not files", () => {
    for (const destination of ["https://example.com/a.md", "https://example.com/a.md:12", "mailto:a@example.com", "tel:123", "javascript:123", "//cdn.example.com/a.md", "file://server/share/a.md", "file://localhost-other/a.md", "#heading", "javascript:alert(1)", ""]) {
      expect(fileReference(destination)).toBeNull();
    }
  });
});

describe("viewer addresses", () => {
  test("a link round-trips through the viewer page's address", () => {
    const url = viewerUrl("/Users/me/My Docs/report.md:12#Next Steps", { session: "s1" })!;
    const address = new URL(url, "http://127.0.0.1:7878/");

    expect(address.pathname).toBe("/");
    expect(viewerTarget(address.search, address.hash)).toEqual({
      path: "/Users/me/My Docs/report.md",
      line: 12,
      fragment: "Next Steps",
      session: "s1",
    });
  });

  test("links inside a viewed file resolve against that file", () => {
    const url = viewerUrl("../img/notes.md", { directory: "/tmp/scratch" })!;
    const address = new URL(url, "http://127.0.0.1:7878/");

    expect(viewerTarget(address.search, address.hash)?.path).toBe("/tmp/scratch/../img/notes.md");
    const encoded = new URL(viewerUrl("linked/../a%23b.md:12", { directory: "/tmp/%41" })!, address);
    expect(viewerTarget(encoded.search, encoded.hash)).toEqual({
      path: "/tmp/%41/linked/../a#b.md", line: 12, fragment: "", session: undefined,
    });
  });

  test("the application's own address is not a viewer page", () => {
    expect(viewerTarget("", "#k=token")).toBeNull();
    expect(viewerTarget("?other=1", "")).toBeNull();
  });

  test("remote links get no viewer page", () => {
    expect(viewerUrl("https://example.com", {})).toBeNull();
  });
});

describe("local images", () => {
  test("local destinations load from the instance and remote ones never load", () => {
    expect(imageSource("/tmp/My%20Shot.png", { session: "s 1" })).toBe("./api/file?path=%2Ftmp%2FMy+Shot.png&session=s+1");
    expect(imageSource("img/a.png", { directory: "/tmp" })).toBe("./api/file?path=%2Ftmp%2Fimg%2Fa.png");
    expect(imageSource("https://example.com/a.png", {})).toBeNull();
    expect(imageSource("data:image/png;base64,AAAA", {})).toBeNull();
    expect(imageSource("file://server/share/a.png", {})).toBeNull();
  });
});

test("Markdown files are recognized by extension", () => {
  expect(isMarkdown("/a/REPORT.MD")).toBe(true);
  expect(isMarkdown("notes.markdown")).toBe(true);
  expect(isMarkdown("main.rs")).toBe(false);
});
