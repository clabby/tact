/**
 * A spinning globe drawn with dots on a Fibonacci sphere, used as the "working" indicator. It
 * tilts diagonally and shows depth through dot size and brightness only. Keep the dot count low
 * (a few dozen): a fine lattice looks the same at every angle, so its rotation cannot be seen. Every sphere shares one
 * animation clock, which stops when none is on the page. The colour is the canvas's CSS `color`.
 */

type Point = readonly [number, number, number];

/** Points spread almost evenly over the unit sphere (the golden-angle spiral). */
export function spherePoints(count: number): Point[] {
  const golden = Math.PI * (3 - Math.sqrt(5));
  return Array.from({ length: count }, (_, index) => {
    const y = 1 - (index / (count - 1)) * 2;
    const radius = Math.sqrt(1 - y * y);
    return [Math.cos(golden * index) * radius, y, Math.sin(golden * index) * radius] as const;
  });
}

/** Rotates a point about the vertical axis by `angle`, then tilts the result by `tilt`. */
export function rotate([x, y, z]: Point, angle: number, tilt: number): Point {
  const turnedX = x * Math.cos(angle) + z * Math.sin(angle);
  const depth = -x * Math.sin(angle) + z * Math.cos(angle);
  return [turnedX * Math.cos(tilt) - y * Math.sin(tilt), turnedX * Math.sin(tilt) + y * Math.cos(tilt), depth];
}

const TILT = 0.6;
/** One full turn takes this long. */
const TURN_MS = 1600;

type Sphere = { points: Point[]; dot: number };
const spheres = new Map<HTMLCanvasElement, Sphere>();
let frame = 0;

function draw(canvas: HTMLCanvasElement, { points, dot }: Sphere, angle: number) {
  const context = canvas.getContext("2d")!;
  const size = canvas.width;
  context.clearRect(0, 0, size, size);
  context.fillStyle = getComputedStyle(canvas).color;
  const radius = (size / 2) * 0.82;
  const placed = points.map((point) => rotate(point, angle, TILT)).sort((a, b) => a[2] - b[2]);
  for (const [x, y, z] of placed) {
    const near = (z + 1) / 2;
    // Far dots nearly vanish, so the near hemisphere's dots visibly travel across the face.
    context.globalAlpha = 0.05 + 0.95 * near * near;
    context.beginPath();
    context.arc(size / 2 + x * radius, size / 2 + y * radius, size * dot * (0.5 + 0.8 * near), 0, Math.PI * 2);
    context.fill();
  }
}

function tick(now: number) {
  for (const [canvas, sphere] of spheres) {
    if (!canvas.isConnected) spheres.delete(canvas);
    else draw(canvas, sphere, ((now % TURN_MS) / TURN_MS) * Math.PI * 2);
  }
  frame = spheres.size ? requestAnimationFrame(tick) : 0;
}

/** A sphere `size` CSS pixels wide made of `count` dots; the caller places it and sets its colour. */
export function createSphere(size: number, count: number): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  const scale = Math.max(2, Math.ceil(devicePixelRatio || 1));
  canvas.width = canvas.height = size * scale;
  canvas.style.width = canvas.style.height = size + "px";
  canvas.className = "sphere";
  canvas.setAttribute("aria-hidden", "true");
  const sphere = { points: spherePoints(count), dot: 0.5 / Math.sqrt(count) };
  spheres.set(canvas, sphere);
  draw(canvas, sphere, 0);
  frame ||= requestAnimationFrame(tick);
  return canvas;
}
