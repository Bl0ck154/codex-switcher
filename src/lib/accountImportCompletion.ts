/**
 * A local auth.json import is complete when credentials are persisted and
 * the saved account list is reloaded. Usage API calls for that account must
 * not keep the Add Account modal in its "Adding..." state.
 */
export async function completeImportWithoutBlockingOnUsage<T>(
  importAccount: () => Promise<T>,
  reloadAccounts: () => Promise<unknown>,
  refreshUsage: (account: T) => Promise<unknown>,
  onRefreshError: (error: unknown) => void,
): Promise<T> {
  const saved = await importAccount();
  await reloadAccounts();
  void Promise.resolve().then(() => refreshUsage(saved)).catch(onRefreshError);
  return saved;
}
