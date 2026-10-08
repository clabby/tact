/**
 * Links into a transcript. The fragment carries a login token (`k`) and optionally a target: a
 * session (`s`) and an entry in it (`entry`). The token is a credential and is never part of a
 * copied link; the target survives the login so the app can open it.
 */
export type HashLink = { token: string | null; session: string | null; entry: number | null };

export function parseHashLink(hash: string): HashLink {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  const entry = Number(params.get("entry"));
  return {
    token: params.get("k"),
    session: params.get("s"),
    entry: params.has("entry") && Number.isInteger(entry) && entry >= 0 ? entry : null,
  };
}

/** The fragment with the token removed, or "" when only the token was there. */
export function hashWithoutToken(hash: string) {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  params.delete("k");
  const rest = params.toString();
  return rest ? `#${rest}` : "";
}

export function entryLink(base: { origin: string; pathname: string; search: string }, session: string, entry: number) {
  const params = new URLSearchParams({ s: session, entry: String(entry) });
  return `${base.origin}${base.pathname}${base.search}#${params}`;
}
