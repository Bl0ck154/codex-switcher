// Execute bounded groups rather than starting one concurrent IPC/OAuth
// request for every account. Each group flushes a single React state update.
export async function mapInBatches<T, R>(
  items: readonly T[],
  batchSize: number,
  worker: (item: T) => Promise<R>,
  onBatch: (results: R[]) => void,
  shouldContinue: () => boolean = () => true,
): Promise<void> {
  const size = Math.max(1, Math.floor(batchSize) || 1);
  for (let offset = 0; offset < items.length && shouldContinue(); offset += size) {
    const results = await Promise.all(items.slice(offset, offset + size).map(worker));
    onBatch(results);
  }
}
