export function needsRelogin(error: string | null | undefined): boolean {
  return !!error && /\b401\b|\bunauthorized\b|invalid_grant|invalid_token|token_(?:expired|invalidated|revoked)|refresh_token_(?:reused|expired|invalidated)|(?:refresh|authentication) token[^\n]*(?:expired|invalid|revoked|reused)|missing refresh token/i.test(error);
}


/** Switch errors that require an interactive login instead of retrying a
 * revoked/expired OAuth refresh token. An old-version string is recognized
 * for compatibility with existing error messages. */
export function switchRequiresRelogin(error: string): boolean {
  return error.startsWith("ACCOUNT_RELOGIN_REQUIRED:")
    || error.includes("Codex credentials could not be renewed. Re-login to this account");
}
