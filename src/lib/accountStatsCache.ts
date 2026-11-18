import type { AccountUsageStats } from "../types";

// Only successful, recently displayed charts are retained after scrolling away.
export class AccountStatsCache {
  private readonly records = new Map<string, { stats: AccountUsageStats; savedAt: number }>();

  private readonly maxEntries: number;
  private readonly ttlMs: number;

  constructor(maxEntries = 12, ttlMs = 5 * 60_000) {
    this.maxEntries = maxEntries;
    this.ttlMs = ttlMs;
  }

  get(accountId: string, now = Date.now()): AccountUsageStats | null {
    const record = this.records.get(accountId);
    if (!record) return null;
    if (now - record.savedAt >= this.ttlMs) {
      this.records.delete(accountId);
      return null;
    }
    this.records.delete(accountId);
    this.records.set(accountId, record);
    return record.stats;
  }

  set(stats: AccountUsageStats, now = Date.now()): void {
    if (!stats.available || stats.error || this.maxEntries <= 0) return;
    this.records.delete(stats.account_id);
    this.records.set(stats.account_id, { stats, savedAt: now });
    while (this.records.size > this.maxEntries) {
      const oldest = this.records.keys().next().value;
      if (oldest === undefined) break;
      this.records.delete(oldest);
    }
  }

  delete(accountId: string): void {
    this.records.delete(accountId);
  }

  get size(): number {
    return this.records.size;
  }
}

export const accountStatsCache = new AccountStatsCache();
