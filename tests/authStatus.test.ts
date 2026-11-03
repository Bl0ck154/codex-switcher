import test from "node:test";
import assert from "node:assert/strict";
import { needsRelogin, formatUsageError, isBackgroundAuthBusy, switchRequiresRelogin } from "../src/lib/accountAuth.ts";
import { retryBusyUsage } from "../src/lib/retryBusyUsage.ts";

test("a busy auth operation is not a signed-out account", () => {
  const error = "Background OAuth refresh busy; retry next poll";
  assert.equal(isBackgroundAuthBusy(error), true);
  assert.equal(needsRelogin(error), false);
  assert.equal(formatUsageError(error).title, "Authentication check postponed");
});

test("a generic 401 or temporary refresh error cannot prove logout", () => {
  for (const error of [
    "401 Unauthorized",
    "Could not refresh the saved Codex Switcher session (401 Unauthorized). Please try again.",
    "Could not refresh the saved Codex Switcher session (429 Too Many Requests).",
    "Failed to send token refresh request; outcome unknown",
  ]) {
    assert.equal(needsRelogin(error), false, error);
    assert.notEqual(formatUsageError(error).title, "Switcher sign-in needs renewal");
  }
});

test("terminal refresh errors do require targeted Switcher re-login", () => {
  for (const error of [
    "refresh_token_reused",
    "refresh_token_invalidated",
    "invalid_grant",
    "Codex Switcher's saved session is out of date",
    "Missing refresh token for account",
    "Token refresh did not return a fresh id_token",
  ]) {
    assert.equal(needsRelogin(error), true, error);
    assert.equal(formatUsageError(error).title, "Switcher sign-in needs renewal");
  }
});

test("only terminal switch errors trigger re-login automatically", () => {
  assert.equal(switchRequiresRelogin("ACCOUNT_RELOGIN_REQUIRED: The session cannot renew"), true);
  assert.equal(switchRequiresRelogin("ACCOUNT_REFRESH_RETRYABLE: Authentication service unavailable"), false);
  assert.equal(switchRequiresRelogin("ACCOUNT_SYNC_FAILED: Could not write auth.json"), false);
});

test("busy-lock retry runs at most three queries and only for busy", async () => {
  let calls = 0;
  let sleeps = 0;
  const result = await retryBusyUsage(async () => {
    calls++;
    if (calls < 3) throw new Error("Background OAuth refresh busy; retry next poll");
    return "ok";
  }, async () => { sleeps++; });
  assert.equal(result, "ok");
  assert.equal(calls, 3);
  assert.equal(sleeps, 2);
});

test("terminal refresh rejection is never replayed", async () => {
  let calls = 0;
  await assert.rejects(retryBusyUsage(async () => {
    calls++;
    throw new Error("refresh_token_reused");
  }, async () => {}), /refresh_token_reused/);
  assert.equal(calls, 1);
});

test("busy retry stops after a bounded number of attempts", async () => {
  let calls = 0;
  await assert.rejects(retryBusyUsage(async () => {
    calls++;
    throw new Error("Background OAuth refresh busy");
  }, async () => {}), /Background OAuth refresh busy/);
  assert.equal(calls, 3);
});
