/**
 * Local files that Markdown names: images it embeds and files it links to. Agents write paths as
 * they appear on disk, absolute or relative to the workspace, so these helpers turn a Markdown
 * destination into a request the instance can answer.
 */

/** Where a relative path resolves: a live session's workspace, or the directory of a viewed file. */
export type FileBase = { session?: string; directory?: string };

/** A local file a link names, decoded, with the line or heading to open it at. */
export type FileReference = { path: string; line: number | null; fragment: string };

/** What the viewer page shows, read back from its address. */
export type ViewerTarget = FileReference & { session?: string };

/** Destinations with a scheme other than file://, or protocol-relative, point elsewhere. */
const REMOTE = /^(?!file:\/\/)([a-z][a-z0-9+.-]*:|\/\/)/i;

const FILE_SCHEME = /^file:\/\/(?:localhost)?(?=\/)/i;

/** An editor-style position suffix, as in "src/app.ts:42" or "src/app.ts:42:7". */
const LINE_SUFFIX = /^(.+?):(\d+)(?::\d+)?$/;

const MARKDOWN = /\.(md|markdown|mdown|mkd|mkdn)$/i;

/** Percent-decodes a destination, keeping it as written when it is not valid percent-encoding. */
export function decodeDestination(value: string) {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/** The path a destination names, without a file:// scheme and percent-decoded. */
function localPath(written: string, directory?: string) {
  if (/^file:\/\//i.test(written) && !FILE_SCHEME.test(written)) return null;
  const path = decodeDestination(written.replace(FILE_SCHEME, ""));

  // Let the filesystem resolve dot segments because a preceding component can be a symlink.
  return directory && !path.startsWith("/") ? `${directory.replace(/\/$/, "")}/${path}` : path;
}

/** The local file a Markdown link destination names, or null for remote URLs and bare fragments. */
export function fileReference(destination: string, directory?: string): FileReference | null {
  if (!destination || destination.startsWith("#")) return null;
  const hash = destination.indexOf("#");
  const written = hash < 0 ? destination : destination.slice(0, hash);
  const fragment = hash < 0 ? "" : decodeDestination(destination.slice(hash + 1));
  const position = LINE_SUFFIX.exec(written);

  // A bare filename with a line resembles a URL scheme. Require an extension to disambiguate it.
  const bareFilePosition = position && /^[^/:]+\.[^/:]+$/.test(position[1]!);
  if (REMOTE.test(written) && !bareFilePosition) return null;

  // Split before decoding so an encoded colon remains part of the filename.
  const path = localPath(position ? position[1]! : written, directory);
  const line = position ? Number(position[2]) : null;
  return path ? { path, line, fragment } : null;
}

/** The query that asks the instance for a path in the session's workspace. */
export function fileQuery(path: string, session?: string) {
  const parameters = new URLSearchParams({ path });
  if (session) parameters.set("session", session);
  return parameters.toString();
}

/** The URL an image destination loads from, or null when the page must not load it. */
export function imageSource(destination: string, base: FileBase) {
  if (!destination || REMOTE.test(destination)) return null;
  const path = localPath(destination, base.directory);
  return path ? `./api/file?${fileQuery(path, base.session)}` : null;
}

/** The viewer page for a link destination naming a local file, or null for any other link. */
export function viewerUrl(destination: string, base: FileBase) {
  const reference = fileReference(destination, base.directory);
  if (!reference) return null;
  const parameters = new URLSearchParams({ file: reference.path });
  if (base.session) parameters.set("session", base.session);
  if (reference.line !== null) parameters.set("line", String(reference.line));
  const fragment = reference.fragment ? `#${encodeURIComponent(reference.fragment)}` : "";
  return `./?${parameters}${fragment}`;
}

/** The file a viewer page address names, or null when the address is the application's. */
export function viewerTarget(search: string, hash: string): ViewerTarget | null {
  const parameters = new URLSearchParams(search);
  const path = parameters.get("file");
  if (!path) return null;
  const line = Number(parameters.get("line"));
  return {
    path,
    line: Number.isInteger(line) && line > 0 ? line : null,
    fragment: decodeDestination(hash.replace(/^#/, "")),
    session: parameters.get("session") ?? undefined,
  };
}

/** Whether the viewer renders a file as Markdown rather than showing its text. */
export function isMarkdown(path: string) {
  return MARKDOWN.test(path);
}
