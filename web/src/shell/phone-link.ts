/** Whether an origin's host only names this computer, so another device cannot reach it. */
export function isLocalOrigin(origin: string) {
  try {
    const host = new URL(origin).hostname;
    return host === "localhost" || host.endsWith(".localhost") || host === "[::1]" || host === "::1" || /^127\./.test(host) || host === "0.0.0.0";
  } catch {
    return true;
  }
}

/**
 * The origin another device should use: the configured public origin when there is one, otherwise
 * the origin this page was loaded from (the tunnel's address when the UI is opened through it).
 * Null when neither can be reached from another device.
 */
export function shareableOrigin(publicOrigin: string | null, pageOrigin: string): string | null {
  const origin = (publicOrigin?.trim() || pageOrigin).replace(/\/+$/, "");
  return isLocalOrigin(origin) ? null : origin;
}

/** The sign-in link for an origin; the token travels in the fragment, which is never sent. */
export function signInLink(origin: string, token: string) {
  return `${origin}/#k=${encodeURIComponent(token)}`;
}
