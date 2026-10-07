import qrcode from "qrcode-generator";

/** The dark modules of a QR code for \`text\`, row by row, at the smallest size that fits. */
export function qrModules(text: string): boolean[][] {
  const code = qrcode(0, "M");
  code.addData(text);
  code.make();
  const size = code.getModuleCount();
  return Array.from({ length: size }, (_, row) => Array.from({ length: size }, (_, column) => code.isDark(row, column)));
}

const SVG = "http://www.w3.org/2000/svg";

/** A crisp, scalable QR code with the quiet zone scanners need. */
export function qrSvg(text: string): SVGSVGElement {
  const modules = qrModules(text);
  const quiet = 4;
  const size = modules.length + quiet * 2;
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `0 0 ${size} ${size}`);
  svg.setAttribute("shape-rendering", "crispEdges");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "QR code that opens this Tact on another device");
  const background = document.createElementNS(SVG, "rect");
  background.setAttribute("width", String(size));
  background.setAttribute("height", String(size));
  background.setAttribute("fill", "#fff");
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("fill", "#000");
  path.setAttribute(
    "d",
    modules.flatMap((row, y) => row.map((dark, x) => (dark ? `M${x + quiet} ${y + quiet}h1v1h-1z` : ""))).join(""),
  );
  svg.append(background, path);
  return svg;
}

