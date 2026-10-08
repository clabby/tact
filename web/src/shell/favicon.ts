// Marks the tab's favicon with a red dot when a session needs attention.

/** The dot sits above and to the right of the "t" in the 626-unit favicon artwork. */
const BADGE = '<circle cx="500" cy="125" r="125" fill="#e5484d" stroke="#fff" stroke-width="30"/>';

let icon: HTMLLinkElement | null | undefined;
let original = "";
let badged: Promise<string> | undefined;
let wanted = false;

/** Shows or clears the attention dot. Safe to call on every render: it only touches the page when the state changes. */
export function setAttentionBadge(attention: boolean) {
  if (attention === wanted) return;
  wanted = attention;
  icon ??= document.querySelector<HTMLLinkElement>('link[rel="icon"]');
  if (!icon) return;
  original ||= icon.href;
  if (!attention) {
    icon.href = original;
    return;
  }
  badged ??= fetch(original)
    .then((response) => response.text())
    .then((svg) => 'data:image/svg+xml,' + encodeURIComponent(svg.replace("</svg>", BADGE + "</svg>")));
  void badged.then((href) => {
    if (wanted && icon) icon.href = href;
  }).catch(() => {});
}
