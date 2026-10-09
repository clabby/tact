import { expect, test } from "bun:test";
import { entryLink, hashWithoutToken, parseHashLink } from "./permalink";

test("a link names a session and entry and never carries the login token", () => {
  const link = entryLink({ origin: "https://box.tail.net", pathname: "/", search: "" }, "019a-01", 42);
  expect(link).toBe("https://box.tail.net/#s=019a-01&entry=42");
  expect(parseHashLink(new URL(link).hash)).toEqual({ token: null, session: "019a-01", entry: 42, heading: null });
  expect(parseHashLink(`${new URL(link).hash}&heading=next-steps`).heading).toBe("next-steps");
});

test("the token is consumed while the link target survives", () => {
  expect(parseHashLink("#k=secret&s=abc&entry=7")).toEqual({ token: "secret", session: "abc", entry: 7, heading: null });
  expect(hashWithoutToken("#k=secret&s=abc&entry=7")).toBe("#s=abc&entry=7");
  expect(hashWithoutToken("#k=secret")).toBe("");
  expect(parseHashLink("#entry=x").entry).toBeNull();
});
