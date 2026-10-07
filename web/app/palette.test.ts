import { expect, test } from "bun:test";
import { availableFirst, matchScore, rankCommands, type PaletteCommand } from "./palette";

const command = (title: string, group = "Actions", keywords?: string): PaletteCommand =>
  ({ id: title, title, group, keywords, run() {} });

test("matching is an in-order, case-insensitive subsequence", () => {
  expect(matchScore("nc", "New chat")).not.toBeNull();
  expect(matchScore("hn", "New chat")).toBeNull();
  expect(matchScore("xz", "New chat")).toBeNull();
  expect(matchScore("  ", "anything")).toBe(0);
});

test("word starts and consecutive runs outrank scattered matches", () => {
  expect(matchScore("nc", "New chat")!).toBeGreaterThan(matchScore("nc", "Announce")!);
  expect(matchScore("com", "Compact")!).toBeGreaterThan(matchScore("com", "Close the model")!);
});

test("title matches outrank keyword matches, and ties keep registration order", () => {
  const commands = [command("Edit config", "Settings", "toml"), command("Compact context"), command("Close chat")];
  expect(rankCommands(commands, "toml").map((c) => c.title)).toEqual(["Edit config"]);
  expect(rankCommands(commands, "c")[0]!.title).toBe("Compact context");
  expect(rankCommands(commands, "").map((c) => c.title)).toEqual(commands.map((c) => c.title));
});

test("unavailable commands follow every available one even when they match better", () => {
  const compact = { ...command("Compact context"), unavailable: "Available after the first turn" };
  const commands = [compact, command("Close chat"), command("Edit config", "Settings", "compact")];
  expect(rankCommands(commands, "compact").map((c) => c.title)).toEqual(["Edit config", "Compact context"]);
  expect(availableFirst(commands).map((c) => c.title)).toEqual(["Close chat", "Edit config", "Compact context"]);
});
