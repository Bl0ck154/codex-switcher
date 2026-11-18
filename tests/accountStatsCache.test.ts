import assert from "node:assert/strict";
import test from "node:test";
import { AccountStatsCache } from "../src/lib/accountStatsCache.ts";
import type { AccountUsageStats } from "../src/types/index.ts";

const stats = (accountId: string, available = true, error: string | null = null) =>
  ({ account_id: accountId, available, error } as AccountUsageStats);

test("recent charts are cached with a bounded LRU capacity", () => {
  const cache = new AccountStatsCache(2, 100);
  cache.set(stats("a"), 0);
  cache.set(stats("b"), 1);
  assert.equal(cache.get("a", 2)?.account_id, "a");
  cache.set(stats("c"), 3);
  assert.equal(cache.size, 2);
  assert.equal(cache.get("b", 4), null);
  assert.equal(cache.get("a", 4)?.account_id, "a");
});

test("expired or failed stats are never returned", () => {
  const cache = new AccountStatsCache(2, 100);
  cache.set(stats("a"), 1000);
  assert.equal(cache.get("a", 1099)?.account_id, "a");
  assert.equal(cache.get("a", 1100), null);
  cache.set(stats("failure", false, "no data"), 1200);
  cache.set(stats("error", true, "server error"), 1200);
  assert.equal(cache.size, 0);
});
