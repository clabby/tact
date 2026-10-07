import { expect, test } from "bun:test";
import { rotate, spherePoints } from "./dot-sphere";

test("points lie on the unit sphere and spread over both hemispheres", () => {
  const points = spherePoints(200);
  expect(points).toHaveLength(200);
  for (const [x, y, z] of points) expect(Math.hypot(x, y, z)).toBeCloseTo(1, 9);
  expect(points.filter(([, y]) => y > 0).length).toBeGreaterThan(90);
  expect(points.filter(([, , z]) => z > 0).length).toBeGreaterThan(90);
});

test("rotation keeps points on the sphere and a quarter turn swaps depth and width", () => {
  const [x, y, z] = rotate([1, 0, 0], Math.PI / 2, 0);
  expect(Math.hypot(x, y, z)).toBeCloseTo(1, 9);
  expect(z).toBeCloseTo(-1, 9);
  expect(x).toBeCloseTo(0, 9);
});
