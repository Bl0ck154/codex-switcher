import assert from "node:assert/strict";
import test from "node:test";
import { reconcileAccounts } from "../src/lib/accountState.ts";

const account = (id: string, isActive = false) => ({
  id, name: id, email: `${id}@example.test`, plan_type: "plus",
  subscription_expires_at: null, auth_mode: "chat_g_p_t" as const,
  is_active: isActive, created_at: "2026-01-01", last_used_at: null,
});

test("preserve untouched account references and cached usage", () => {
  const first = { ...account("one"), usage: { error: null } as never, usageLoading: false };
  const second = { ...account("two"), usageLoading: true };
  const previous = [first, second];
  assert.strictEqual(reconcileAccounts(previous, [account("one"), account("two")], true), previous);
  const changed = reconcileAccounts(previous, [account("one"), account("two", true)], true);
  assert.strictEqual(changed[0], first);
  assert.notStrictEqual(changed[1], second);
  assert.equal(changed[1].usageLoading, true);
  assert.equal(changed[1].is_active, true);
});

test("drop deleted account usage and never reuse objects after a full reload", () => {
  const previous = [{ ...account("old"), usageLoading: true }];
  assert.deepEqual(reconcileAccounts(previous, [account("new")], true).map((item) => item.id), ["new"]);
  const reloaded = reconcileAccounts(previous, [account("old")], false);
  assert.notStrictEqual(reloaded[0], previous[0]);
  assert.equal(reloaded[0].usageLoading, false);
});

test("preserve ordering and reconcile rename and metadata changes", () => {
  const a = account("a");
  const b = account("b");
  const previous = [{ ...a }, { ...b }];
  const renamed = { ...a, name: "renamed", plan_type: "pro" };
  const next = reconcileAccounts(previous, [b, renamed], true);
  assert.strictEqual(next[0], previous[1]);
  assert.equal(next[1].name, "renamed");
  assert.equal(next[1].plan_type, "pro");
});
