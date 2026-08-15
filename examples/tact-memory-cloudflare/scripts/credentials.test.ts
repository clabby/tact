import { expect, test } from "bun:test";
import { credentialDocument } from "./credentials";

test("renders multiple credentials as separate server records", () => {
  expect(
    credentialDocument({
      credentials: [
        { namespace: "alice", role: "writer", token: "alice-token" },
        { namespace: "auditor", role: "reader", token: "auditor-token" },
      ],
    }),
  ).toBe("writer alice alice-token\nreader auditor auditor-token");
});

test("rejects duplicate tokens without including them in the error", () => {
  const token = "private-token";
  expect(() =>
    credentialDocument({
      credentials: [
        { namespace: "alice", role: "writer", token },
        { namespace: "bob", role: "reader", token },
      ],
    }),
  ).toThrow("credentials[1].token duplicates an earlier entry");

  try {
    credentialDocument({
      credentials: [
        { namespace: "alice", role: "writer", token },
        { namespace: "bob", role: "reader", token },
      ],
    });
  } catch (error) {
    expect(String(error)).not.toContain(token);
  }
});
