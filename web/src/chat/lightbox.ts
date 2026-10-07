import { glyph } from "../ui/glyphs";

/**
 * Shows an image large over the page. A click on the backdrop or the close button, or Escape,
 * dismisses it; a click on the image toggles between fitting the window and its real size.
 */
export function openLightbox(source: string, alt: string) {
  const restore = document.activeElement as HTMLElement | null;
  const dialog = document.createElement("dialog");
  dialog.className = "lightbox";
  dialog.setAttribute("aria-label", alt || "Image");
  dialog.innerHTML = `<button type="button" class="icon-button lightbox-close" aria-label="Close">${glyph("close")}</button>
    <div class="lightbox-stage"><img></div>
    <p class="lightbox-caption" hidden></p>`;
  const image = dialog.querySelector("img")!;
  image.src = source;
  image.alt = alt;
  const caption = dialog.querySelector<HTMLElement>(".lightbox-caption")!;
  if (alt) {
    caption.textContent = alt;
    caption.hidden = false;
  }
  const close = () => dialog.close();
  dialog.querySelector(".lightbox-close")!.addEventListener("click", close);
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog || (event.target as HTMLElement).classList.contains("lightbox-stage")) close();
  });
  image.addEventListener("click", () => dialog.classList.toggle("actual"));
  dialog.addEventListener("close", () => {
    dialog.remove();
    restore?.focus({ preventScroll: true });
  });
  document.body.append(dialog);
  dialog.showModal();
}

