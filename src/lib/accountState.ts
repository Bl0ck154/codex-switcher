import type { AccountInfo, AccountWithUsage } from "../types";

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
