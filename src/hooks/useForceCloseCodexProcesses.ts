import { useCallback, useState } from "react";
import type { CodexProcessInfo } from "../types";
import { invokeBackend } from "../lib/platform";

interface KillCodexProcessesResult {
  targeted_count: number;
  killed_pids: number[];
  failed_pids: number[];
  reopen_token?: string | null;
}

interface UseForceCloseCodexProcessesOptions {
  processCount: number;
  checkProcesses: () => Promise<CodexProcessInfo | null>;
  showToast: (message: string, isError?: boolean) => void;
  formatError: (err: unknown) => string;
}

export function useForceCloseCodexProcesses({
  processCount,
  checkProcesses,
  showToast,
  formatError,
}: UseForceCloseCodexProcessesOptions) {
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [isForceClosing, setIsForceClosing] = useState(false);

  const forceCloseCodexProcesses = useCallback(async (reopenDesktop = false) => {
    try {
      setIsForceClosing(true);

      const result = await invokeBackend<KillCodexProcessesResult>(
        "kill_codex_processes",
        { reopenDesktop }
      );
      const latestProcessInfo = await checkProcesses();
      const remainingCount = latestProcessInfo?.count ?? processCount;
      const closedCount = Math.max(0, processCount - remainingCount);

      if (!latestProcessInfo) {
        showToast("Could not verify that Codex closed. Account switching and reopening were skipped.", true);
      } else if (result.targeted_count === 0) {
        showToast("No running Codex processes found.");
      } else if (remainingCount === 0) {
        showToast(
          `Force closed ${processCount} Codex session${
            processCount === 1 ? "" : "s"
          }.`
        );
      } else if (closedCount > 0) {
        showToast(
          `Force closed ${closedCount}/${processCount} Codex sessions. ${remainingCount} still running.`,
          true
        );
      } else {
        showToast(
          `Could not force close ${remainingCount} Codex session${
            remainingCount === 1 ? "" : "s"
          }.`,
          true
        );
      }

      return { processInfo: latestProcessInfo, reopenToken: result.reopen_token ?? null };
    } catch (err) {
      console.error("Failed to force close Codex processes:", err);
      showToast(`Force close failed: ${formatError(err)}`, true);
      return null;
    } finally {
      setConfirmOpen(false);
      setIsForceClosing(false);
    }
  }, [checkProcesses, formatError, processCount, showToast]);

  return {
    forceCloseConfirmOpen: confirmOpen,
    setForceCloseConfirmOpen: setConfirmOpen,
    isForceClosingCodex: isForceClosing,
    forceCloseCodexProcesses,
  };
}
