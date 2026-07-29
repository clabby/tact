export const REVIEW_SETTINGS_KEY = "tact.review.settings.v1";
const REVIEW_SETTINGS_COOKIE = "tact_review_settings_v1";

export type SyntaxTheme =
  | "system"
  | "pierre-light"
  | "pierre-light-soft"
  | "pierre-dark"
  | "pierre-dark-soft";

export type ReviewSettings = {
  syntaxTheme: SyntaxTheme;
  diffStyle: "unified" | "split";
  wrapLines: boolean;
  lineNumbers: boolean;
};

export const DEFAULT_REVIEW_SETTINGS: ReviewSettings = {
  syntaxTheme: "system",
  diffStyle: "unified",
  wrapLines: false,
  lineNumbers: true,
};

type SettingsStorage = Pick<Storage, "getItem" | "setItem">;

export function loadReviewSettings(storage: SettingsStorage, cookies = ""): ReviewSettings {
  try {
    const value = storage.getItem(REVIEW_SETTINGS_KEY) ?? readSettingsCookie(cookies);
    if (!value) return { ...DEFAULT_REVIEW_SETTINGS };
    const settings = validateSettings(JSON.parse(value));
    storage.setItem(REVIEW_SETTINGS_KEY, JSON.stringify(settings));
    return settings;
  } catch {
    return { ...DEFAULT_REVIEW_SETTINGS };
  }
}

export function saveReviewSettings(
  storage: SettingsStorage,
  settings: ReviewSettings,
  writeCookie?: (cookie: string) => void,
) {
  const value = JSON.stringify(settings);
  try {
    storage.setItem(REVIEW_SETTINGS_KEY, value);
  } catch {
    // The review remains fully usable when storage is unavailable.
  }
  try {
    writeCookie?.(
      `${REVIEW_SETTINGS_COOKIE}=${encodeURIComponent(value)}; Path=/; SameSite=Strict; Max-Age=31536000`,
    );
  } catch {
    // localStorage remains the fallback for this review origin.
  }
}

export function diffTheme(settings: ReviewSettings) {
  if (settings.syntaxTheme === "system") {
    return { light: "pierre-light", dark: "pierre-dark" } as const;
  }
  return settings.syntaxTheme;
}

export function activeSyntaxTheme(settings: ReviewSettings, prefersDark: boolean) {
  if (settings.syntaxTheme !== "system") return settings.syntaxTheme;
  return prefersDark ? "pierre-dark" : "pierre-light";
}

export function appearance(settings: ReviewSettings): "light" | "dark" | "system" {
  if (settings.syntaxTheme === "system") return "system";
  return settings.syntaxTheme.includes("dark") ? "dark" : "light";
}

function validateSettings(value: unknown): ReviewSettings {
  if (!value || typeof value !== "object") return { ...DEFAULT_REVIEW_SETTINGS };
  const candidate = value as Partial<ReviewSettings>;
  const syntaxThemes: SyntaxTheme[] = [
    "system",
    "pierre-light",
    "pierre-light-soft",
    "pierre-dark",
    "pierre-dark-soft",
  ];
  return {
    syntaxTheme: syntaxThemes.includes(candidate.syntaxTheme as SyntaxTheme)
      ? candidate.syntaxTheme as SyntaxTheme
      : DEFAULT_REVIEW_SETTINGS.syntaxTheme,
    diffStyle: candidate.diffStyle === "split" ? "split" : "unified",
    wrapLines: typeof candidate.wrapLines === "boolean"
      ? candidate.wrapLines
      : DEFAULT_REVIEW_SETTINGS.wrapLines,
    lineNumbers: typeof candidate.lineNumbers === "boolean"
      ? candidate.lineNumbers
      : DEFAULT_REVIEW_SETTINGS.lineNumbers,
  };
}

function readSettingsCookie(cookies: string): string | undefined {
  const prefix = `${REVIEW_SETTINGS_COOKIE}=`;
  const value = cookies
    .split(";")
    .map((cookie) => cookie.trim())
    .find((cookie) => cookie.startsWith(prefix))
    ?.slice(prefix.length);
  if (!value) return undefined;
  return decodeURIComponent(value);
}
