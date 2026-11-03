/**
 * A busy background auth-operation lock means no refresh request was sent.
 * Retrying the usage query is safe; never retry a failed OAuth token POST.
 */
export async function retryBusyUsage<T>(
  request: () => Promise<T>,
  pause: (ms: number) => Promise<void> = (ms) =>
    new Promise((resolve) => setTimeout(resolve, ms)),
): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    try {
      return await request();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!/Background OAuth refresh busy/i.test(message) || attempt >= 2) {
        throw error;
      }
      await pause(300 * (attempt + 1));
    }
  }
}
