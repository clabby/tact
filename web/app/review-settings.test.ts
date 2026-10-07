import { describe, expect, test } from "bun:test";
import {
  DEFAULT_REVIEW_SETTINGS,
  REVIEW_SETTINGS_KEY,
  activeSyntaxTheme,
  diffTheme,
  loadReviewSettings,
  saveReviewSettings,
} from "./review-settings";

class MemoryStorage {
  private readonly values = new Map<string, string>();

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

describe("review settings", () => {
  test("wraps diff lines by default", () => {
    expect(loadReviewSettings(new MemoryStorage()).wrapLines).toBe(true);
  });

  test("invalid persisted values fall back field by field", () => {
    const storage = new MemoryStorage();
    storage.setItem(REVIEW_SETTINGS_KEY, JSON.stringify({
      syntaxTheme: "unknown",
      diffStyle: "split",
      wrapLines: true,
      lineNumbers: "yes",
    }));

    expect(loadReviewSettings(storage)).toEqual({
      ...DEFAULT_REVIEW_SETTINGS,
      diffStyle: "split",
      wrapLines: true,
    });
  });

  test("saved settings round trip", () => {
    const storage = new MemoryStorage();
    const settings = {
      ...DEFAULT_REVIEW_SETTINGS,
      syntaxTheme: "pierre-dark-soft" as const,
    };

    saveReviewSettings(storage, settings);

    expect(loadReviewSettings(storage)).toEqual(settings);
  });

  test("a cookie carries settings across ephemeral review ports", () => {
    const firstOrigin = new MemoryStorage();
    const secondOrigin = new MemoryStorage();
    const settings = {
      ...DEFAULT_REVIEW_SETTINGS,
      diffStyle: "split" as const,
      wrapLines: true,
    };
    let cookie = "";

    saveReviewSettings(firstOrigin, settings, (value) => {
      cookie = value;
    });

    expect(loadReviewSettings(secondOrigin, cookie)).toEqual(settings);
    expect(loadReviewSettings(secondOrigin)).toEqual(settings);
  });

  test("system uses matching Pierre themes for diffs and previews", () => {
    expect(diffTheme(DEFAULT_REVIEW_SETTINGS)).toEqual({
      light: "pierre-light",
      dark: "pierre-dark",
    });
    expect(activeSyntaxTheme(DEFAULT_REVIEW_SETTINGS, false)).toBe("pierre-light");
    expect(activeSyntaxTheme(DEFAULT_REVIEW_SETTINGS, true)).toBe("pierre-dark");
  });
});
