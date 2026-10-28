import type { SessionNote } from "../core/wire";

/**
 * The monitor model scores how well each session's work is going from 0 to 100, in five bands it
 * is told to use. The gauge fills one segment per band, so its fill reads as the band the model
 * chose: five for delivered or verified progress, one for stuck with no way forward.
 */
const BANDS = [
  { min: 90, segments: 5, label: "delivering", tone: "success" },
  { min: 70, segments: 4, label: "on track", tone: "success" },
  { min: 40, segments: 3, label: "slow or uncertain", tone: "warning" },
  { min: 20, segments: 2, label: "struggling", tone: "danger" },
  { min: 0, segments: 1, label: "stuck", tone: "danger" },
] as const;

/** A score's band: the gauge's filled segments out of five, its colour, and what it means. */
export type HealthReading = Omit<(typeof BANDS)[number], "min">;

/** How far a score must move between checks before the gauge marks the change. */
export const TREND_STEP = 10;

export function healthReading(score: number): HealthReading {
  const { segments, label, tone } = BANDS.find(({ min }) => score >= min) ?? BANDS[BANDS.length - 1]!;
  return { segments, label, tone };
}

/** Whether the score rose or fell by at least `TREND_STEP` since the previous check. */
export function healthTrend({ health, previous_health: previous }: Pick<SessionNote, "health" | "previous_health">): "up" | "down" | null {
  if (health === null || previous === null) return null;
  if (health - previous >= TREND_STEP) return "up";
  if (previous - health >= TREND_STEP) return "down";
  return null;
}
