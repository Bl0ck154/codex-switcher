/** An access-token 401 alone is not proof that a refresh token was revoked. */
export function needsRelogin(error: string | null | undefined): boolean {
  if (!error) return false;
  return /ACCOUNT_RELOGIN_REQUIRED:|refresh_token_(?:reused|expired|invalidated)|invalid_grant|token_invalidated|authentication token[^\\n]*(?:expired|invalid|revoked)|saved session is out of date|outdated refresh token|invalid refresh token|refresh token is invalid|missing refresh token|did not return a fresh id_token|invalid or expired id_token|your session has ended/i.test(error);
}

/** A background auth lock is a busy state, not an authentication failure. */
export function isBackgroundAuthBusy(error: string | null | undefined): boolean {
  return !!error && /Background OAuth refresh busy/i.test(error);
}

/** Keep OAuth failures distinct from rate-limit and transient usage errors. */
export function formatUsageError(error: string): { title: string; message: string } {
  if (isBackgroundAuthBusy(error)) {
    return {
      title: "Authentication check postponed",
      message: "Another OAuth operation is running. Switcher will retry; this does not mean the account is signed out.",
    };
  }

  if (needsRelogin(error)) {
    return {
      title: "Switcher sign-in needs renewal",
      message: /refresh_token_reused|outdated refresh token|saved session is out of date/i.test(error)
        ? "The saved refresh token may already have been rotated by Codex or another client. Other apps can still work; re-login to this account in Switcher to restore switching."
        : "Switcher cannot renew its saved credentials. This does not prove the account is signed out in other apps. Re-login in Switcher to restore switching.",
    };
  }

  if (/failed to parse usage response/i.test(error)) {
    return {
      title: "Usage format changed",
      message: "The account may still be signed in. Switcher couldn't read the usage response.",
    };
  }

  if (/could not refresh the saved codex switcher session|failed to send token refresh request|401 unauthorized|429 too many requests/i.test(error)) {
    return {
      title: "Authentication check unsuccessful",
      message: "The request failed, but the saved refresh token has not been proven invalid. Retry before re-authenticating.",
    };
  }

  return { title: "Usage unavailable", message: error };
}

/** Only a verified terminal switch failure starts the targeted re-login flow. */
export function switchRequiresRelogin(error: string): boolean {
  return error.startsWith("ACCOUNT_RELOGIN_REQUIRED:")
    || error.includes("Codex credentials could not be renewed. Re-login to this account");
}
