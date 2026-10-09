import { ApiError } from "./api-client";

/**
 * Machine mode. A page opened with `?m=<name>` runs everything about a session on a linked machine:
 * the hub relays the machine's API under `./api/m/<name>`, while sign-in, the machine list, the
 * phone link, and sibling instances stay on the hub.
 */

/** The names the hub registers machines under; the hub refuses any other. */
const MACHINE_NAME = /^[a-z0-9][a-z0-9-]{0,31}$/;

export function isMachineName(name: string) {
  return MACHINE_NAME.test(name);
}

/** The machine the page was opened for, or null for the hub's own machine. */
export function machineFromSearch(search: string): string | null {
  return new URLSearchParams(search).get("m");
}

/** The API root of a registered machine; `name` must satisfy `isMachineName`. */
export function machineBase(name: string) {
  return `./api/m/${name}`;
}

/** The page to load to work on `machine` (null: the hub's own machine), as a fresh app without a link target. */
export function machineUrl(href: string, machine: string | null) {
  const url = new URL(href);
  if (machine === null) url.searchParams.delete("m");
  else url.searchParams.set("m", machine);
  url.hash = "";
  return url.href;
}

type KeyValueStorage = Pick<Storage, "getItem" | "setItem">;

/**
 * Storage whose keys are private to `machine`. Session ids are chosen by whoever runs the machine,
 * so keys derived from them must not be shared across machines. Names contain no `:`, so a suffix
 * identifies its machine whatever precedes it.
 */
export function machineStorage(storage: KeyValueStorage, machine: string | null): KeyValueStorage {
  if (machine === null) return storage;
  const suffix = `:m:${machine}`;
  return {
    getItem: (key) => storage.getItem(key + suffix),
    setItem: (key, value) => storage.setItem(key + suffix, value),
  };
}

/** How a failed call to a machine must be presented, when it is not an ordinary error. */
export function machineFailure(error: unknown): "relink" | "unregistered" | null {
  if (!(error instanceof ApiError)) return null;
  if (error.code === "machine_unauthorized") return "relink";
  if (error.code === "unknown_machine") return "unregistered";
  return null;
}
