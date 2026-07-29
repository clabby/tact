export type OverviewAppearance = "light" | "dark" | "system";

export function overviewDocument(html: string, appearance: OverviewAppearance) {
  return `<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
  </head>
  <body>
    <main class="agent-overview">${html}</main>
    <style>${overviewStyles(appearance)}</style>
  </body>
</html>`;
}

function overviewStyles(appearance: OverviewAppearance) {
  const colorScheme = appearance === "system" ? "light dark" : appearance;
  return `
    :root {
      color-scheme: ${colorScheme};
      --overview-bg: light-dark(#ffffff, #181b18);
      --overview-surface: light-dark(#f0f1ef, #282b29);
      --overview-line: light-dark(#e1e3df, #353936);
      --overview-text: light-dark(#242624, #e8eae8);
      --overview-muted: light-dark(#5d625d, #abb0ab);
      --overview-accent: light-dark(#315f36, #9bc59e);
      font: 15px/1.65 Inter, ui-sans-serif, system-ui, sans-serif;
      color: var(--overview-text);
      background: var(--overview-bg);
    }
    body { margin: 0; color: var(--overview-text); background: var(--overview-bg); }
    .agent-overview { max-width: 780px; margin: 0 auto; padding: 56px 42px 120px; }
    .agent-overview, .agent-overview * {
      color: inherit !important;
      border-color: var(--overview-line) !important;
      text-decoration-color: currentColor !important;
    }
    .agent-overview * { background-color: transparent !important; }
    .agent-overview h1, .agent-overview h2, .agent-overview h3 {
      line-height: 1.2;
      letter-spacing: -.025em;
      margin: 2em 0 .6em;
    }
    .agent-overview h1:first-child, .agent-overview h2:first-child { margin-top: 0; }
    .agent-overview p, .agent-overview ul, .agent-overview ol,
    .agent-overview pre, .agent-overview table { margin: 0 0 1.2em; }
    .agent-overview code {
      font: 13px ui-monospace, SFMono-Regular, monospace;
      background: var(--overview-surface) !important;
      padding: 2px 5px;
      border-radius: 5px;
    }
    .agent-overview pre {
      padding: 18px;
      overflow: auto;
      background: var(--overview-surface) !important;
      border: 1px solid var(--overview-line);
      border-radius: 10px;
    }
    .agent-overview pre code { padding: 0; background: transparent !important; }
    .agent-overview a { color: var(--overview-accent) !important; }
    .agent-overview strong { font-weight: 650; }
    .agent-overview blockquote {
      margin: 1.5em 0;
      padding-left: 18px;
      border-left: 2px solid var(--overview-accent);
      color: var(--overview-muted) !important;
    }
    .agent-overview table { width: 100%; border-collapse: collapse; }
    .agent-overview th, .agent-overview td { padding: 8px 10px; border-bottom: 1px solid var(--overview-line); }
    .agent-overview svg [fill]:not([fill="none"]) { fill: currentColor !important; }
    .agent-overview svg [stroke]:not([stroke="none"]) { stroke: currentColor !important; }
  `;
}
