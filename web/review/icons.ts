import {
  icon as renderFontAwesomeIcon,
  type IconDefinition,
} from "@fortawesome/fontawesome-svg-core";
import {
  faArrowRight,
  faBold,
  faCheck,
  faChevronDown,
  faChevronRight,
  faCircle,
  faCircleCheck,
  faCode,
  faCodeBranch,
  faComment,
  faEllipsis,
  faFile,
  faGear,
  faItalic,
  faLink,
  faList,
  faLock,
  faPen,
  faQuoteLeft,
  faTerminal,
  faTrashCan,
  faWandMagicSparkles,
  faXmark,
} from "@fortawesome/free-solid-svg-icons";
import type { FileTreeIcons } from "@pierre/trees";

const icons = {
  "arrow-right": faArrowRight,
  bold: faBold,
  check: faCheck,
  "chevron-down": faChevronDown,
  close: faXmark,
  code: faCode,
  "code-block": faTerminal,
  edit: faPen,
  "git-branch": faCodeBranch,
  italic: faItalic,
  link: faLink,
  list: faList,
  quote: faQuoteLeft,
  settings: faGear,
  sparkles: faWandMagicSparkles,
  trash: faTrashCan,
} as const;

export type ReviewIconName = keyof typeof icons;
export type FormattingIconName = Extract<
  ReviewIconName,
  "bold" | "italic" | "code" | "code-block" | "link" | "list" | "quote"
>;

export function icon(name: ReviewIconName) {
  return renderFontAwesomeIcon(icons[name], {
    attributes: {
      "aria-hidden": "true",
      focusable: "false",
    },
  }).html.join("");
}

export const treeIcons = {
  set: "none",
  colored: false,
  spriteSheet: spriteSheet({
    "tact-fa-chevron-right": faChevronRight,
    "tact-fa-circle": faCircle,
    "tact-fa-ellipsis": faEllipsis,
    "tact-fa-file": faFile,
    "tact-fa-lock": faLock,
  }),
  remap: {
    "file-tree-icon-chevron": remappedIcon("tact-fa-chevron-right", faChevronRight),
    "file-tree-icon-dot": remappedIcon("tact-fa-circle", faCircle),
    "file-tree-icon-ellipsis": remappedIcon("tact-fa-ellipsis", faEllipsis),
    "file-tree-icon-file": remappedIcon("tact-fa-file", faFile),
    "file-tree-icon-lock": remappedIcon("tact-fa-lock", faLock),
  },
} satisfies FileTreeIcons;

export const commentIconMask = iconMask(faComment);
export const seenIconMask = iconMask(faCircleCheck);

function iconMask(definition: IconDefinition) {
  const svg = renderFontAwesomeIcon(definition).html.join("");
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

function remappedIcon(name: string, definition: IconDefinition) {
  const [width, height] = definition.icon;
  return { name, width: 16, height: 16, viewBox: `0 0 ${width} ${height}` };
}

function spriteSheet(definitions: Record<string, IconDefinition>) {
  const symbols = Object.entries(definitions).map(([name, definition]) => {
    const [width, height, , , pathData] = definition.icon;
    const paths = (Array.isArray(pathData) ? pathData : [pathData])
      .map((path) => `<path fill="currentColor" d="${path}"/>`)
      .join("");
    return `<symbol id="${name}" viewBox="0 0 ${width} ${height}">${paths}</symbol>`;
  });
  return `<svg xmlns="http://www.w3.org/2000/svg" aria-hidden="true" style="display:none">${symbols.join("")}</svg>`;
}
