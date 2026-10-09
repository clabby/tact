import { glyph } from "../ui/glyphs";

export type LightboxImage = { source: string; alt: string };

const MAX_ZOOM = 8;

/**
 * Shows images large over the page, starting at `images[start]`. The arrow buttons and the Left
 * and Right keys step through the set. The wheel, the zoom buttons, + and -, or a double click
 * zoom in about the pointer; a zoomed image pans by dragging, and 0 fits it again. The backdrop,
 * the close button, and Escape dismiss it.
 */
export function openLightbox(images: LightboxImage[], start = 0) {
  if (!images.length) return;
  const restore = document.activeElement as HTMLElement | null;
  const dialog = document.createElement("dialog");
  dialog.className = "lightbox";
  dialog.innerHTML = `<div class="lightbox-bar">
      <span class="lightbox-count"></span>
      <div class="lightbox-zoom" role="group" aria-label="Zoom">
        <button type="button" class="icon-button" data-zoom="out" aria-label="Zoom out" title="Zoom out (-)">${glyph("minus")}</button>
        <button type="button" class="lightbox-level" data-zoom="fit" title="Fit to window (0)"></button>
        <button type="button" class="icon-button" data-zoom="in" aria-label="Zoom in" title="Zoom in (+)">${glyph("plus")}</button>
      </div>
      <button type="button" class="icon-button lightbox-close" aria-label="Close" title="Close (Esc)">${glyph("close")}</button>
    </div>
    <div class="lightbox-stage"><img draggable="false"></div>
    <button type="button" class="lightbox-nav" data-step="-1" aria-label="Previous image" title="Previous (←)">${glyph("chevron-left")}</button>
    <button type="button" class="lightbox-nav" data-step="1" aria-label="Next image" title="Next (→)">${glyph("chevron-right")}</button>
    <p class="lightbox-caption" hidden></p>`;
  const stage = dialog.querySelector<HTMLElement>(".lightbox-stage")!;
  const image = dialog.querySelector("img")!;
  const caption = dialog.querySelector<HTMLElement>(".lightbox-caption")!;
  const count = dialog.querySelector<HTMLElement>(".lightbox-count")!;
  const level = dialog.querySelector<HTMLElement>(".lightbox-level")!;
  const previous = dialog.querySelector<HTMLButtonElement>('[data-step="-1"]')!;
  const next = dialog.querySelector<HTMLButtonElement>('[data-step="1"]')!;
  dialog.classList.toggle("single", images.length === 1);

  let index = Math.min(Math.max(0, start), images.length - 1);
  // The image's transform: a zoom relative to its fitted size, and an offset from the centre.
  let zoom = 1;
  let x = 0;
  let y = 0;

  const apply = () => {
    image.style.transform = `translate(${x}px, ${y}px) scale(${zoom})`;
    dialog.classList.toggle("zoomed", zoom > 1);
    const natural = image.naturalWidth ? image.offsetWidth * zoom / image.naturalWidth : zoom;
    level.textContent = `${Math.round(natural * 100)}%`;
  };
  /** Zooms to `target`, keeping the stage point (clientX, clientY), or the centre, still. */
  const zoomTo = (target: number, clientX?: number, clientY?: number) => {
    const next = Math.min(MAX_ZOOM, Math.max(1, target));
    const box = stage.getBoundingClientRect();
    const centreX = box.left + box.width / 2;
    const centreY = box.top + box.height / 2;
    const px = (clientX ?? centreX) - centreX;
    const py = (clientY ?? centreY) - centreY;
    x = px - (px - x) * next / zoom;
    y = py - (py - y) * next / zoom;
    zoom = next;
    if (zoom === 1) x = y = 0;
    apply();
  };
  const show = (to: number) => {
    index = Math.min(Math.max(0, to), images.length - 1);
    const { source, alt } = images[index]!;
    zoom = 1;
    x = y = 0;
    image.src = source;
    image.alt = alt;
    dialog.setAttribute("aria-label", alt || "Image");
    caption.textContent = alt;
    caption.hidden = !alt;
    count.textContent = images.length > 1 ? `${index + 1} / ${images.length}` : "";
    previous.disabled = index === 0;
    next.disabled = index === images.length - 1;
    apply();
  };
  image.addEventListener("load", apply);

  dialog.querySelector(".lightbox-close")!.addEventListener("click", () => dialog.close());
  previous.addEventListener("click", () => show(index - 1));
  next.addEventListener("click", () => show(index + 1));
  for (const button of dialog.querySelectorAll<HTMLElement>("[data-zoom]")) {
    button.addEventListener("click", () => {
      const action = button.dataset.zoom;
      zoomTo(action === "fit" ? 1 : action === "in" ? zoom * 1.5 : zoom / 1.5);
    });
  }
  dialog.addEventListener("keydown", (event) => {
    if (event.key === "ArrowLeft") show(index - 1);
    else if (event.key === "ArrowRight") show(index + 1);
    else if (event.key === "+" || event.key === "=") zoomTo(zoom * 1.5);
    else if (event.key === "-") zoomTo(zoom / 1.5);
    else if (event.key === "0") zoomTo(1);
    else return;
    event.preventDefault();
  });
  stage.addEventListener("wheel", (event) => {
    event.preventDefault();
    zoomTo(zoom * Math.exp(-event.deltaY * 0.002), event.clientX, event.clientY);
  }, { passive: false });
  image.addEventListener("dblclick", (event) => zoomTo(zoom > 1 ? 1 : 2.5, event.clientX, event.clientY));

  // One pointer drags a zoomed image or, at fit, swipes to the neighbouring one; a press that
  // barely moves on the backdrop closes the lightbox.
  let drag: { id: number; startX: number; startY: number; x: number; y: number; moved: boolean } | null = null;
  stage.addEventListener("pointerdown", (event) => {
    if (drag || event.button !== 0) return;
    drag = { id: event.pointerId, startX: event.clientX, startY: event.clientY, x, y, moved: false };
    stage.setPointerCapture(event.pointerId);
  });
  stage.addEventListener("pointermove", (event) => {
    if (drag?.id !== event.pointerId) return;
    const dx = event.clientX - drag.startX;
    const dy = event.clientY - drag.startY;
    drag.moved ||= Math.hypot(dx, dy) > 4;
    if (zoom > 1) {
      x = drag.x + dx;
      y = drag.y + dy;
      apply();
    }
  });
  stage.addEventListener("pointerup", (event) => {
    if (drag?.id !== event.pointerId) return;
    const { startX, moved } = drag;
    drag = null;
    const dx = event.clientX - startX;
    if (zoom === 1 && moved && Math.abs(dx) > 60) show(index + (dx < 0 ? 1 : -1));
    else if (!moved && event.target === stage) dialog.close();
  });
  stage.addEventListener("pointercancel", () => { drag = null; });
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog) dialog.close();
  });
  dialog.addEventListener("close", () => {
    dialog.remove();
    restore?.focus({ preventScroll: true });
  });
  document.body.append(dialog);
  show(index);
  dialog.showModal();
  // Keys work from the dialog itself, without a focus ring on whichever button came first.
  dialog.tabIndex = -1;
  dialog.focus();
}
