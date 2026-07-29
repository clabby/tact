export type OverviewAppearance = "light" | "dark" | "system";

export function overviewDocument(html: string, appearance: OverviewAppearance) {
  return `<!doctype html>
<html data-theme="${appearance}">
  <head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
    <style>${overviewStyles(appearance)}</style>
  </head>
  <body>
    ${html}
  </body>
</html>`;
}

function overviewStyles(appearance: OverviewAppearance) {
  const colorScheme = appearance === "system" ? "light dark" : appearance;
  return `
    :root {
      color-scheme: ${colorScheme};
      --overview-bg: light-dark(#ffffff, #171a20);
      background: var(--overview-bg);
    }
    html, body { min-height: 100%; background: var(--overview-bg); }
    body { margin: 0; }
  `;
}
