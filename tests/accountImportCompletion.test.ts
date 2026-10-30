import assert from "node:assert/strict";
import test from "node:test";
import { completeImportWithoutBlockingOnUsage } from "../src/lib/accountImportCompletion.ts";

test("file import finishes without waiting for slow usage for all accounts", async () => {
  const saved = { id: "imported" };
  const calls: string[] = [];
  const result = await completeImportWithoutBlockingOnUsage(
    async () => { calls.push("import"); return saved; },
    async () => { calls.push("reload"); },
    async (account) => {
      assert.equal(account, saved);
      calls.push("usage");
      return new Promise<void>(() => {});
    },
    () => assert.fail("unexpected refresh error"),
  );
  assert.equal(result, saved);
  assert.deepEqual(calls, ["import", "reload", "usage"]);
});
test("import errors remain visible and never refresh usage", async () => {
  const error = Error("account replacement requires confirmation");
  let refreshed = false;
  await assert.rejects(
    completeImportWithoutBlockingOnUsage(
      async () => { throw error; },
      async () => undefined,
      async () => { refreshed = true; },
      () => undefined,
    ),
    (got) => got === error,
  );
  assert.equal(refreshed, false);
});
test("background usage errors cannot turn a saved import into an error", async () => {
  const failures: unknown[] = [];
  const failure = Error("usage request unavailable");
  const saved = await completeImportWithoutBlockingOnUsage(
    async () => ({ id: "saved" }),
    async () => undefined,
    async () => { throw failure; },
    (err) => failures.push(err),
  );
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(saved.id, "saved");
  assert.deepEqual(failures, [failure]);
});
