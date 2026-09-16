import { expect, test } from "bun:test";
import { activate, type CommandResult } from "./activate";

function successfulVerification(): CommandResult {
  return {
    exitCode: 0,
    stdout: JSON.stringify([
      { success: true, results: [] },
      { success: true, results: [{ verified: 1 }] },
    ]),
  };
}

test("applies and verifies local migrations without deploying", async () => {
  const commands: string[][] = [];
  const results = [{ exitCode: 0, stdout: "" }, successfulVerification()];

  await activate("local", async (command) => {
    commands.push(command);
    return results.shift()!;
  });

  expect(commands).toHaveLength(2);
  expect(commands[0]).toEqual([
    "bun",
    "x",
    "wrangler",
    "d1",
    "migrations",
    "apply",
    "DB",
    "--local",
  ]);
  expect(commands[1]).toContain("--local");
  expect(commands[1]).toContain("--json");
  const sql = commands[1][commands[1].indexOf("--command") + 1];
  expect(sql).toContain("memories_search_insert");
  expect(sql).toContain("memory_search_documents");
  expect(sql).toContain("integrity-check");
});

test("deploys only after remote migration and verification", async () => {
  const commands: string[][] = [];
  const results = [
    { exitCode: 0, stdout: "" },
    successfulVerification(),
    { exitCode: 0, stdout: "" },
  ];

  await activate("deploy", async (command) => {
    commands.push(command);
    return results.shift()!;
  });

  expect(commands.map((command) => command.slice(3, 6))).toEqual([
    ["d1", "migrations", "apply"],
    ["d1", "execute", "DB"],
    ["deploy"],
  ]);
  expect(commands[0]).toContain("--remote");
  expect(commands[1]).toContain("--remote");
});

test("remote migration does not implicitly deploy", async () => {
  const commands: string[][] = [];
  const results = [{ exitCode: 0, stdout: "" }, successfulVerification()];

  await activate("remote", async (command) => {
    commands.push(command);
    return results.shift()!;
  });

  expect(commands).toHaveLength(2);
  expect(commands.every((command) => command.includes("--remote"))).toBeTrue();
});

test("forwards isolated local database options to migration and verification", async () => {
  const commands: string[][] = [];
  const results = [{ exitCode: 0, stdout: "" }, successfulVerification()];

  await activate(
    "local",
    async (command) => {
      commands.push(command);
      return results.shift()!;
    },
    { config: "/tmp/test-wrangler.json", persistTo: "/tmp/test-state" },
  );

  for (const command of commands) {
    expect(command).toContain("--config");
    expect(command).toContain("/tmp/test-wrangler.json");
    expect(command).toContain("--persist-to");
    expect(command).toContain("/tmp/test-state");
  }
});

test("rejects local persistence for remote activation", async () => {
  const commands: string[][] = [];

  await expect(
    activate(
      "remote",
      async (command) => {
        commands.push(command);
        return { exitCode: 0, stdout: "" };
      },
      { persistTo: "/tmp/test-state" },
    ),
  ).rejects.toThrow("--persist-to is supported only for local activation");

  expect(commands).toHaveLength(0);
});

test("does not verify or deploy after a failed migration", async () => {
  const commands: string[][] = [];

  await expect(
    activate("deploy", async (command) => {
      commands.push(command);
      return { exitCode: 1, stdout: "" };
    }),
  ).rejects.toThrow("exited with status 1");

  expect(commands).toHaveLength(1);
});

test("does not deploy after an inconsistent index", async () => {
  const commands: string[][] = [];
  const results = [
    { exitCode: 0, stdout: "" },
    { exitCode: 0, stdout: JSON.stringify([{ success: true, results: [{ verified: 0 }] }]) },
  ];

  await expect(
    activate("deploy", async (command) => {
      commands.push(command);
      return results.shift()!;
    }),
  ).rejects.toThrow("database schema or search index verification failed");

  expect(commands).toHaveLength(2);
});

test("does not deploy when Wrangler reports success false", async () => {
  const commands: string[][] = [];
  const results = [
    { exitCode: 0, stdout: "" },
    { exitCode: 0, stdout: JSON.stringify([{ success: false, results: [] }]) },
  ];

  await expect(
    activate("deploy", async (command) => {
      commands.push(command);
      return results.shift()!;
    }),
  ).rejects.toThrow("wrangler reported that verification failed");

  expect(commands).toHaveLength(2);
});
