import { expect, test } from "bun:test";
import { firstLine, formatAge, formatDuration, formatTokens, statusLabel } from "./format";

test("durations print like the TUI", () => {
  expect([840e6, 4.2e9, 42e9, 402e9, 3_900e9].map(formatDuration)).toEqual(["840ms", "4.2s", "42s", "6m 42s", "1h 05m"]);
});

test("ages are relative to now", () => {
  const now = 10_000_000_000;
  expect(formatAge(now - 10_000, now)).toBe("now");
  expect(formatAge(now - 5 * 60_000, now)).toBe("5m");
  expect(formatAge(now - 3 * 3_600_000, now)).toBe("3h");
  expect(formatAge(now - 2 * 86_400_000, now)).toBe("2d");
});

test("token counts are compact", () => {
  expect([950, 1_234, 12_400, 1_200_000].map(formatTokens)).toEqual(["950", "1.2k", "12k", "1.2M"]);
});

test("previews keep the first line within the limit", () => {
  expect(firstLine("  hello\nworld")).toBe("hello");
  expect(firstLine("abcdef", 4)).toBe("abc…");
});

test("retry status names the attempt", () => {
  expect(statusLabel(null)).toBeNull();
  expect(statusLabel({ kind: "retrying", delay_ns: 2e9, next_attempt: 2, max_attempts: 5 } as never))
    .toBe("Retrying in 2.0s · attempt 2 of 5");
});
