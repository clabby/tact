import { expect, test } from "bun:test";
import { qrModules } from "./qr";

test("a QR code is a square with the three finder patterns", () => {
  const modules = qrModules("http://100.64.0.1:7878/#k=token");
  const size = modules.length;
  expect(modules.every((row) => row.length === size)).toBe(true);
  expect(size).toBeGreaterThanOrEqual(21);
  // The top-left finder pattern starts with seven dark modules.
  expect(modules[0]!.slice(0, 7).every(Boolean)).toBe(true);
  expect(modules[0]![size - 7]).toBe(true);
  expect(modules[size - 7]![0]).toBe(true);
});
