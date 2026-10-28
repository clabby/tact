import { expect, test } from "bun:test";
import { healthReading, healthTrend } from "./health";

test("each health band fills its own number of segments in its colour", () => {
  const read = (score: number) => {
    const { segments, tone } = healthReading(score);
    return [score, segments, tone];
  };
  expect([100, 90, 89, 70, 69, 40, 39, 20, 19, 0].map(read)).toEqual([
    [100, 5, "success"], [90, 5, "success"],
    [89, 4, "success"], [70, 4, "success"],
    [69, 3, "warning"], [40, 3, "warning"],
    [39, 2, "danger"], [20, 2, "danger"],
    [19, 1, "danger"], [0, 1, "danger"],
  ]);
});

test("a trend shows only when the score moved by ten or more since the previous check", () => {
  const trend = (health: number | null, previous_health: number | null) => healthTrend({ health, previous_health });
  expect([trend(80, 71), trend(80, 70), trend(70, 80), trend(71, 80), trend(80, null), trend(null, 80)])
    .toEqual([null, "up", "down", null, null, null]);
});
