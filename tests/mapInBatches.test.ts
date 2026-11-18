import assert from "node:assert/strict";
import test from "node:test";
import { mapInBatches } from "../src/lib/mapInBatches.ts";

test("bounded batches preserve ordering and reduce state flushes", async () => {
  const seen: number[][] = [];
  await mapInBatches([1, 2, 3, 4, 5, 6, 7], 3, async (x) => x * 2, (batch) => {
    seen.push(batch);
  });
  assert.deepEqual(seen, [[2, 4, 6], [8, 10, 12], [14]]);
});

test("next batch waits for the previous batch and empty input is a no-op", async () => {
  const pending: Array<() => void> = [];
  const started: number[] = [];
  const completed: number[][] = [];
  const promise = mapInBatches([1, 2, 3], 2, (item) => new Promise<number>((resolve) => {
    started.push(item);
    pending.push(() => resolve(item));
  }), (batch) => completed.push(batch));

  assert.deepEqual(started, [1, 2]);
  pending.shift()?.();
  pending.shift()?.();
  await Promise.resolve();
  await Promise.resolve();
  assert.deepEqual(started, [1, 2, 3]);
  pending.shift()?.();
  await promise;
  assert.deepEqual(completed, [[1, 2], [3]]);
  await mapInBatches([], 2, async (item) => item, () => assert.fail("unexpected flush"));
});
