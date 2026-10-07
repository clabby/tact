import { describe, expect, test } from "bun:test";
import { parsePlan } from "./plan-detail";

describe("parsePlan", () => {
  test("keeps step order and statuses, defaulting unknown statuses to pending", () => {
    const plan = parsePlan({
      explanation: "Reordered",
      plan: [
        { step: "a", status: "completed" },
        { step: "b", status: "in_progress" },
        { step: "c", status: "someday" },
        { status: "pending" },
      ],
    });
    expect(plan).toEqual({
      explanation: "Reordered",
      steps: [
        { step: "a", status: "completed" },
        { step: "b", status: "in_progress" },
        { step: "c", status: "pending" },
      ],
    });
  });

  test("rejects arguments without a plan list", () => {
    expect(parsePlan(null)).toBeNull();
    expect(parsePlan("text")).toBeNull();
    expect(parsePlan({ plan: "x" })).toBeNull();
  });
});
