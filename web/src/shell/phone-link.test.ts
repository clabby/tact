import { expect, test } from "bun:test";
import { isLocalOrigin, shareableOrigin, signInLink } from "./phone-link";

test("loopback origins are not shareable", () => {
  for (const origin of ["http://127.0.0.1:7878", "http://localhost:7878", "http://[::1]:7878", "http://0.0.0.0:7878"]) {
    expect(isLocalOrigin(origin)).toBe(true);
  }
  expect(isLocalOrigin("https://laptop.tail1234.ts.net")).toBe(false);
  expect(isLocalOrigin("http://100.64.1.2:7878")).toBe(false);
});

test("the configured public origin wins, then the page's own origin", () => {
  expect(shareableOrigin("https://pub.example.net/", "http://127.0.0.1:7878")).toBe("https://pub.example.net");
  expect(shareableOrigin(null, "https://laptop.ts.net")).toBe("https://laptop.ts.net");
  expect(shareableOrigin("", "http://100.64.1.2:7878")).toBe("http://100.64.1.2:7878");
});

test("without a reachable origin there is no link to share", () => {
  expect(shareableOrigin(null, "http://localhost:7878")).toBeNull();
  expect(shareableOrigin("not a url", "http://localhost:7878")).toBeNull();
});

test("a phone signs in on the machine the page works on", () => {
  expect(signInLink("https://h.ts.net", "a/b", "devbox")).toBe("https://h.ts.net/?m=devbox#k=a%2Fb");
});

test("the token goes in the fragment", () => {
  expect(signInLink("https://h.ts.net", "a/b")).toBe("https://h.ts.net/#k=a%2Fb");
});

