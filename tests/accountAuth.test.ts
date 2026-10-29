import assert from "node:assert/strict";
import test from "node:test";
import { needsRelogin, switchRequiresRelogin } from "../src/lib/accountAuth.ts";

test("re-login is offered for authentication failures, not quota or network errors", () => {
  for (const message of [
    "API error: 401 Unauthorized", "HTTP 401", "invalid_grant",
    "refresh_token_reused", "refresh_token_expired", "token_invalidated",
    "Missing refresh token for account", "Your authentication token has been invalidated",
  ]) assert.equal(needsRelogin(message), true, message);
  for (const message of [null, undefined, "", "API error: 403 Forbidden", "429 Too Many Requests",
    "usage_limit_exceeded", "Network timeout", "invalid JSON", "request ID 40123 failed"])
    assert.equal(needsRelogin(message), false, String(message));
});

test("switch errors offer re-login only for authentication failures", () => {
  for (const message of [
    "ACCOUNT_RELOGIN_REQUIRED: The saved ChatGPT session cannot be renewed.",
    "Codex credentials could not be renewed. Re-login to this account before switching.",
  ]) assert.equal(switchRequiresRelogin(message), true, message);

  for (const message of [
    "ACCOUNT_REFRESH_RETRYABLE: Authentication service is unavailable.",
    "ACCOUNT_SYNC_FAILED: Credentials could not be written.",
    "API error 429",
  ]) assert.equal(switchRequiresRelogin(message), false, message);
});
