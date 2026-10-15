import { beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { SessionTitles, TITLE_REVEAL_MS } from "./session-title";

beforeAll(() => {
  Object.assign(globalThis, { document: new Window().document });
});

function harness() {
  let now = 0;
  const titles = new SessionTitles(() => now);
  const element = document.createElement("h1") as unknown as HTMLElement;
  return {
    element,
    render: (session: string, text: string, pending: boolean) => titles.render(element, session, text, pending),
    advance: (ms: number) => { now += ms; },
    classes: () => [...element.classList].sort(),
  };
}

describe("session titles", () => {
  test("a pending title shimmers and its final title reveals once", () => {
    const title = harness();
    title.render("a", "Fix the parser", true);
    expect(title.classes()).toEqual(["title-pending"]);
    title.render("a", "Parser repair", false);
    expect(title.element.textContent).toBe("Parser repair");
    expect(title.classes()).toEqual(["title-reveal"]);
    title.advance(TITLE_REVEAL_MS);
    title.render("a", "Parser repair", false);
    expect(title.classes()).toEqual([]);
    expect(title.element.style.animationDelay).toBe("");
  });

  test("initial renders, rerenders, and changes between final titles swap instantly", () => {
    const title = harness();
    title.render("a", "Parser repair", false);
    title.render("a", "Parser repair", false);
    title.render("a", "Renamed", false);
    expect(title.classes()).toEqual([]);
  });

  test("another session's title does not inherit a running reveal", () => {
    const title = harness();
    title.render("a", "Fix the parser", true);
    title.render("a", "Parser repair", false);
    title.render("b", "Docs", false);
    expect(title.classes()).toEqual([]);
  });

  test("a rebuilt element resumes a running reveal where it was", () => {
    let now = 0;
    const titles = new SessionTitles(() => now);
    const first = document.createElement("span") as unknown as HTMLElement;
    titles.render(first, "a", "Fix the parser", true);
    titles.render(first, "a", "Parser repair", false);
    now = 100;
    const rebuilt = document.createElement("span") as unknown as HTMLElement;
    titles.render(rebuilt, "a", "Parser repair", false);
    expect(rebuilt.classList.contains("title-reveal")).toBe(true);
    expect(rebuilt.style.animationDelay).toBe("-100ms");
  });
});
