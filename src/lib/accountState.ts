import type { AccountInfo, AccountWithUsage, UsageInfo } from "../types";

const ACCOUNT_FIELDS: (keyof AccountInfo)[] = [
  "id", "name", "email", "plan_type", "subscription_expires_at",
  "auth_mode", "is_active", "created_at", "last_used_at",
];

// Reuse unchanged objects so an accounts-changed event does not update every card.
// Do not preserve usage on a full reload (e.g. after a backup import).
export function reconcileAccounts(
  previous: AccountWithUsage[],
  incoming: AccountInfo[],
  preserveUsage: boolean,
): AccountWithUsage[] {
  const byId = new Map(previous.map((account) => [account.id, account]));
  const next = incoming.map((account) => {
    const old = preserveUsage ? byId.get(account.id) : undefined;
    if (old && ACCOUNT_FIELDS.every((field) => old[field] === account[field])) {
      return old;
    }
    return {
      ...account,
      usage: old?.usage,
      usageLoading: old?.usageLoading ?? false,
    };
  });
  return next.length === previous.length &&
    next.every((account, index) => account === previous[index])
    ? previous
    : next;
}

const USAGE_FIELDS: (Exclude<keyof UsageInfo, "luna_reserve">)[] = [
  "account_id", "plan_type", "primary_used_percent", "primary_window_minutes",
  "primary_resets_at", "secondary_used_percent", "secondary_window_minutes",
  "secondary_resets_at", "has_credits", "unlimited_credits", "credits_balance", "error",
];

// A new IPC object need not invalidate every card if the displayed quota did
// not change. Compare the optional reserve separately instead of stringifying.
export function sameUsage(previous: UsageInfo | undefined, incoming: UsageInfo): boolean {
  if (previous === incoming) return true;
  if (!previous || !USAGE_FIELDS.every((field) => previous[field] === incoming[field])) {
    return false;
  }
  const a = previous.luna_reserve;
  const b = incoming.luna_reserve;
  if (a == null || b == null) return a == null && b == null;
  return a.normal_model_slug === b.normal_model_slug &&
    a.allowed === b.allowed &&
    a.limit_reached === b.limit_reached &&
    a.used_percent === b.used_percent &&
    a.window_minutes === b.window_minutes &&
    a.resets_at === b.resets_at;
}
