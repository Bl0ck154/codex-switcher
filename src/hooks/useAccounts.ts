import { useState, useEffect, useCallback, useRef } from "react";
import type {
  AccountInfo,
  UsageInfo,
  AccountWithUsage,
  WarmupSummary,
  ImportAccountsSummary,
} from "../types";
import { invokeBackend, isTauriRuntime, type FileSource } from "../lib/platform";
import { completeOAuthWithoutBlockingOnUsage } from "../lib/oauthCompletion";
import { completeImportWithoutBlockingOnUsage } from "../lib/accountImportCompletion";
import { retryBusyUsage } from "../lib/retryBusyUsage";
import { reconcileAccounts, sameUsage } from "../lib/accountState";

export function useAccounts() {
  const [accounts, setAccounts] = useState<AccountWithUsage[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const accountsRef = useRef<AccountWithUsage[]>([]);
  const metadataRefreshInFlightRef = useRef(new Set<string>());
  // OAuth refresh is serialized; parallel usage requests starve each other on the auth lock.
  const maxConcurrentUsageRequests = 1;
  const usageRequestDelayMs = 150;

  useEffect(() => {
    accountsRef.current = accounts;
  }, [accounts]);

  const buildUsageError = useCallback(
    (accountId: string, message: string, planType: string | null): UsageInfo => ({
      account_id: accountId,
      plan_type: planType,
      primary_used_percent: null,
      primary_window_minutes: null,
      primary_resets_at: null,
      secondary_used_percent: null,
      secondary_window_minutes: null,
      secondary_resets_at: null,
      has_credits: null,
      unlimited_credits: null,
      credits_balance: null,
      error: message,
    }),
    []
  );

  // Push freshly polled usage down to the tray (single poller feeds the tray menu).
  const reportUsageToTray = useCallback((usages: UsageInfo[]) => {
    if (!isTauriRuntime() || usages.length === 0) return;
    void invokeBackend("report_usage", { usages }).catch(() => {});
  }, []);

  const runWithConcurrency = useCallback(
    async <T,>(
      items: T[],
      worker: (item: T) => Promise<void>,
      concurrency: number,
      delayMs = 0
    ) => {
      if (items.length === 0) return;
      const limit = Math.min(Math.max(concurrency, 1), items.length);
      let index = 0;
      const runners = Array.from({ length: limit }, async () => {
        while (true) {
          const current = index++;
          if (current >= items.length) return;
          if (delayMs > 0 && current > 0) {
            await new Promise((resolve) => setTimeout(resolve, delayMs));
          }
          await worker(items[current]);
        }
      });
      await Promise.allSettled(runners);
    },
    []
  );

  const loadAccounts = useCallback(async (preserveUsage = false) => {
    try {
      setLoading(true);
      setError(null);
      const accountList = await invokeBackend<AccountInfo[]>("list_accounts");
      
      setAccounts((prev) => reconcileAccounts(prev, accountList, preserveUsage));
      return accountList;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return [];
    } finally {
      setLoading(false);
    }
  }, []);

  const refreshMetadata = useCallback(
    async (
      accountList?: AccountInfo[] | AccountWithUsage[]
    ) => {
      const list = accountList ?? accountsRef.current;
      const dueAccounts = list.filter(
        (account) => !metadataRefreshInFlightRef.current.has(account.id)
      );

      // Mark attempts before starting requests so overlapping refresh cycles
      // cannot issue duplicate metadata calls for the same account.
      dueAccounts.forEach((account) => {
        metadataRefreshInFlightRef.current.add(account.id);
      });

      const metadataById = new Map<string, AccountInfo>();
      await runWithConcurrency(
        dueAccounts,
        async (account) => {
          try {
            const metadata = await invokeBackend<AccountInfo>("refresh_account_metadata", {
              accountId: account.id,
            });
            metadataById.set(account.id, metadata);
          } catch (err) {
            console.warn("Failed to refresh account metadata:", err);
          } finally {
            metadataRefreshInFlightRef.current.delete(account.id);
          }
        },
        1,
        250
      );

      if (metadataById.size) {
        // One batched state update, not N full-list updates for N accounts.
        setAccounts((prev) => {
          let changed = false;
          const next = prev.map((item) => {
            const metadata = metadataById.get(item.id);
            if (!metadata || (
              item.plan_type === metadata.plan_type &&
              item.subscription_expires_at === metadata.subscription_expires_at
            )) return item;
            changed = true;
            return {
              ...item,
              plan_type: metadata.plan_type,
              subscription_expires_at: metadata.subscription_expires_at,
            };
          });
          return changed ? next : prev;
        });
      }
    },
    [runWithConcurrency]
  );

  const refreshUsage = useCallback(
    async (
      accountList?: AccountInfo[] | AccountWithUsage[],
      options?: { refreshMetadata?: boolean; silent?: boolean }
    ) => {
      try {
        const list = accountList ?? accountsRef.current;
        if (list.length === 0) {
          return;
        }

        const shouldRefreshMetadata = Boolean(options?.refreshMetadata);
        const accountIds = list.map((account) => account.id);
        const accountIdSet = new Set(accountIds);
        const usageResults = new Map<string, UsageInfo>();

        if (!options?.silent) {
          setAccounts((prev) =>
            prev.map((account) =>
              accountIdSet.has(account.id) && !account.usageLoading
                ? { ...account, usageLoading: true }
                : account
            )
          );
        }

        await runWithConcurrency(
          list,
          async (account) => {
            try {
              const usage = await retryBusyUsage(() => invokeBackend<UsageInfo>("get_usage", {
                accountId: account.id,
              }));
              usageResults.set(account.id, usage);
            } catch (err) {
              console.error("Failed to refresh usage:", err);
              const message = err instanceof Error ? err.message : String(err);
              usageResults.set(
                account.id,
                buildUsageError(account.id, message, account.plan_type ?? null)
              );
            }
          },
          maxConcurrentUsageRequests,
          usageRequestDelayMs
        );

        setAccounts((prev) =>
          prev.map((account) => {
            const usage = usageResults.get(account.id);
            if (!usage) return account;
            if (sameUsage(account.usage, usage) && account.usageLoading === false) {
              return account;
            }
            return {
              ...account,
              usage: sameUsage(account.usage, usage) ? account.usage : usage,
              usageLoading: false,
            };
          })
        );

        reportUsageToTray(Array.from(usageResults.values()));

        // Run metadata refresh sequentially after usage updates,
        // so we don't bombard Cloudflare with parallel requests across endpoints.
        if (shouldRefreshMetadata) {
          await refreshMetadata(list);
        }
      } catch (err) {
        console.error("Failed to refresh usage:", err);
        throw err;
      }
    },
    [
      buildUsageError,
      maxConcurrentUsageRequests,
      refreshMetadata,
      reportUsageToTray,
      runWithConcurrency,
      usageRequestDelayMs,
    ]
  );

  const refreshSingleUsage = useCallback(async (
    accountId: string,
    options?: { refreshMetadata?: boolean }
  ) => {
    try {
      const account = accountsRef.current.find((item) => item.id === accountId);
      const metadataPromise = options?.refreshMetadata && account
        ? refreshMetadata([account])
        : Promise.resolve();

      setAccounts((prev) =>
        prev.map((a) =>
          a.id === accountId ? { ...a, usageLoading: true } : a
        )
      );
      const usage = await retryBusyUsage(() => invokeBackend<UsageInfo>("get_usage", { accountId }));
      setAccounts((prev) =>
        prev.map((a) =>
          a.id === accountId ? { ...a, usage, usageLoading: false } : a
        )
      );
      reportUsageToTray([usage]);
      await metadataPromise;
      return usage;
    } catch (err) {
      console.error("Failed to refresh single usage:", err);
      const message = err instanceof Error ? err.message : String(err);
      setAccounts((prev) =>
        prev.map((a) =>
          a.id === accountId
            ? {
                ...a,
                usage: buildUsageError(accountId, message, a.plan_type ?? null),
                usageLoading: false,
              }
            : a
        )
      );
      throw err;
    }
  }, [buildUsageError, refreshMetadata, reportUsageToTray]);

  const warmupAccount = useCallback(async (accountId: string) => {
    try {
      await invokeBackend("warmup_account", { accountId });
    } catch (err) {
      console.error("Failed to warm up account:", err);
      throw err;
    }
  }, []);

  const warmupAllAccounts = useCallback(async () => {
    try {
      return await invokeBackend<WarmupSummary>("warmup_all_accounts");
    } catch (err) {
      console.error("Failed to warm up all accounts:", err);
      throw err;
    }
  }, []);

  const switchAccount = useCallback(
    async (accountId: string, expectedActiveId?: string) => {
      try {
        await invokeBackend(expectedActiveId ? "switch_account_if_current" : "switch_account", { accountId, ...(expectedActiveId ? { expectedActiveId } : {}) });
        await loadAccounts(true); // Preserve existing usage while updating account state.
        // A successful switch may have been preceded by an old usage/auth error.
        // Refresh that account without delaying the completed switch.
        void refreshSingleUsage(accountId).catch((error) => {
          console.warn("Failed to refresh usage after switching:", error);
        });
      } catch (err) {
        throw err;
      }
    },
    [loadAccounts, refreshSingleUsage]
  );

  const deleteAccount = useCallback(
    async (accountId: string) => {
      try {
        await invokeBackend("delete_account", { accountId });
        // Account activation can change while deletion is in flight. Re-read
        // backend metadata without discarding the latest cached usage.
        await loadAccounts(true);
      } catch (err) {
        throw err;
      }
    },
    [loadAccounts]
  );

  const renameAccount = useCallback(
    async (accountId: string, newName: string) => {
      try {
        await invokeBackend("rename_account", { accountId, newName });
        await loadAccounts(true); // Preserve usage data
      } catch (err) {
        throw err;
      }
    },
    [loadAccounts]
  );

  const importFromFile = useCallback(
    (source: FileSource, name: string, forceReplace = false) =>
      completeImportWithoutBlockingOnUsage(
        async () => {
          if (typeof source === "string") {
            return invokeBackend<AccountInfo>("add_account_from_file", { path: source, name, forceReplace });
          }
          const contents = await source.text();
          return invokeBackend<AccountInfo>("add_account_from_auth_json_text", {
            name,
            contents,
            forceReplace,
          });
        },
        () => loadAccounts(true),
        (account) => refreshUsage([account]),
        (err) => console.warn("Failed to refresh imported account usage:", err),
      ),
    [loadAccounts, refreshUsage]
  );

  const startOAuthLogin = useCallback(async (accountName: string, reauthAccountId?: string) => {
    try {
      const info = await invokeBackend<{ auth_url: string; callback_port: number }>(
        "start_login",
        { accountName, reauthAccountId }
      );
      return info;
    } catch (err) {
      throw err;
    }
  }, []);

  const completeOAuthLogin = useCallback((forceReplace = false) =>
    completeOAuthWithoutBlockingOnUsage(
      () => invokeBackend<AccountInfo>("complete_login", { forceReplace }),
      () => loadAccounts(true),
      (account) => refreshUsage([account]),
      (err) => console.warn("Failed to refresh usage after OAuth login:", err),
    ), [loadAccounts, refreshUsage]);

  const exportAccountsSlimText = useCallback(async () => {
    try {
      return await invokeBackend<string>("export_accounts_slim_text");
    } catch (err) {
      throw err;
    }
  }, []);

  const importAccountsSlimText = useCallback(
    async (payload: string) => {
      try {
        const summary = await invokeBackend<ImportAccountsSummary>("import_accounts_slim_text", {
          payload,
        });
        const accountList = await loadAccounts();
        await refreshUsage(accountList);
        return summary;
      } catch (err) {
        throw err;
      }
    },
    [loadAccounts, refreshUsage]
  );

  const exportAccountsFullEncryptedFile = useCallback(
    async (path: string) => {
      try {
        await invokeBackend("export_accounts_full_encrypted_file", { path });
      } catch (err) {
        throw err;
      }
    },
    []
  );

  const importAccountsFullEncryptedFile = useCallback(
    async (path: string) => {
      try {
        const summary = await invokeBackend<ImportAccountsSummary>(
          "import_accounts_full_encrypted_file",
          { path }
        );
        const accountList = await loadAccounts();
        await refreshUsage(accountList);
        return summary;
      } catch (err) {
        throw err;
      }
    },
    [loadAccounts, refreshUsage]
  );

  const cancelOAuthLogin = useCallback(async () => {
    try {
      await invokeBackend("cancel_login");
    } catch (err) {
      console.error("Failed to cancel login:", err);
      throw err;
    }
  }, []);

  const loadMaskedAccountIds = useCallback(async () => {
    try {
      return await invokeBackend<string[]>("get_masked_account_ids");
    } catch (err) {
      console.error("Failed to load masked account IDs:", err);
      return [];
    }
  }, []);

  const saveMaskedAccountIds = useCallback(async (ids: string[]) => {
    try {
      await invokeBackend("set_masked_account_ids", { ids });
    } catch (err) {
      console.error("Failed to save masked account IDs:", err);
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    let usageTimer: ReturnType<typeof setTimeout> | undefined;

    // This poll is needed even while the window is hidden: auto warm-up reads
    // current quotas from the same account state. Serialize cycles instead of
    // stacking requests if a large account list takes over 60 seconds to load.
    const schedulePoll = () => {
      if (!cancelled) usageTimer = setTimeout(() => void pollUsage(), 60_000);
    };
    const pollUsage = async () => {
      try {
        // Suppress per-card loading animations for an unfocused/hidden main
        // window while still feeding fresh quotas to automatic warm-up.
        const silent = typeof document !== "undefined" &&
          (document.visibilityState === "hidden" || !document.hasFocus());
        await refreshUsage(undefined, { silent });
      } catch (error) {
        console.warn("Automatic usage refresh failed:", error);
      } finally {
        schedulePoll();
      }
    };

    void (async () => {
      const accountList = await loadAccounts();
      if (cancelled) return;
      try {
        await refreshUsage(accountList);
      } catch (error) {
        console.warn("Initial usage refresh failed:", error);
      } finally {
        if (!cancelled) {
          // Expiry fetch remains sequential behind the initial quota fetch.
          void refreshMetadata(accountList);
          schedulePoll();
        }
      }
    })();

    const metadataInterval = !isTauriRuntime()
      ? setInterval(() => {
          void refreshMetadata().catch(() => {});
        }, 6 * 60 * 60 * 1000)
      : undefined;

    return () => {
      cancelled = true;
      if (usageTimer !== undefined) clearTimeout(usageTimer);
      if (metadataInterval !== undefined) clearInterval(metadataInterval);
    };
  }, [loadAccounts, refreshMetadata, refreshUsage]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    void (async () => {
      if (!isTauriRuntime()) return;
      const { listen } = await import("@tauri-apps/api/event");
      const unsubscribe = await listen("accounts-changed", () => {
        void loadAccounts(true);
      });
      // The asynchronous subscription may resolve after unmount / StrictMode cleanup.
      if (disposed) unsubscribe();
      else unlisten = unsubscribe;
    })().catch((error) => console.warn("Failed to listen for account updates:", error));

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadAccounts]);

  return {
    accounts,
    loading,
    error,
    loadAccounts,
    refreshUsage,
    refreshSingleUsage,
    warmupAccount,
    warmupAllAccounts,
    switchAccount,
    deleteAccount,
    renameAccount,
    importFromFile,
    exportAccountsSlimText,
    importAccountsSlimText,
    exportAccountsFullEncryptedFile,
    importAccountsFullEncryptedFile,
    startOAuthLogin,
    completeOAuthLogin,
    cancelOAuthLogin,
    loadMaskedAccountIds,
    saveMaskedAccountIds,
  };
}
