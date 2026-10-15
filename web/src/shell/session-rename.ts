import { describeError, type ApiClient } from "../core/api-client";
import { openSheet } from "../ui/sheet";

export function openSessionRename(api: ApiClient, session: string, title: string) {
  const sheet = openSheet("Rename session");
  sheet.body.innerHTML = `<form class="session-rename">
    <label class="session-rename-label">Session title<input type="text" name="title" required autocomplete="off"></label>
    <p class="sheet-notice" data-tone="danger" role="alert" hidden></p>
    <div class="sheet-footer"><button type="button" class="button cancel">Cancel</button><button type="submit" class="button primary">Save</button></div>
  </form>`;
  const form = sheet.body.querySelector("form")!;
  const input = form.querySelector("input")!;
  const save = form.querySelector<HTMLButtonElement>('[type="submit"]')!;
  const notice = form.querySelector<HTMLElement>(".sheet-notice")!;
  input.value = title;
  input.focus();
  input.select();
  form.querySelector(".cancel")!.addEventListener("click", sheet.close);
  input.addEventListener("input", () => {
    save.disabled = !input.value.trim();
    notice.hidden = true;
  });
  save.disabled = !input.value.trim();
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const title = input.value.trim();
    if (!title || save.disabled) return;
    save.disabled = true;
    input.disabled = true;
    try {
      await api.command("rename_session", { session, title });
      sheet.close();
    } catch (error) {
      notice.textContent = describeError(error);
      notice.hidden = false;
      save.disabled = false;
      input.disabled = false;
    }
  });
}
