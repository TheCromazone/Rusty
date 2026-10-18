const assert = require("assert");
const path = require("path");
const { fetchAll } = require(path.resolve("fetch_all.js"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  assert.deepStrictEqual(await fetchAll([], async () => 1), []);
  // Hold two initial calls open and release a third. The pool must refill
  // before the held calls finish, regardless of machine speed.
  let live = 0, peak = 0;
  const started = [];
  const ids = Array.from({ length: 12 }, (_, i) => i);
  const gates = ids.map(() => {
    let release;
    const promise = new Promise((resolve) => { release = resolve; });
    return { promise, release };
  });
  const pending = fetchAll(ids, async (i) => {
    started.push(i);
    live++; peak = Math.max(peak, live);
    await gates[i].promise;
    live--;
    return `r${i}`;
  }, { concurrency: 3 });
  // setImmediate lets every queued promise continuation finish without a
  // wall-clock threshold or assumptions about the order calls start in.
  await new Promise(setImmediate);
  assert.strictEqual(started.length, 3, "fill the available pool slots");
  gates[started[1]].release();
  await new Promise(setImmediate);
  assert.strictEqual(started.length, 4, "refill while earlier calls are still pending (no fixed batches)");
  for (const gate of gates) gate.release();
  const out = await pending;
  assert.deepStrictEqual(out, ids.map((i) => `r${i}`));
  assert.strictEqual(peak, 3, `peak ${peak}`);
  // Default concurrency is 4.
  live = 0; peak = 0;
  await fetchAll(ids, async () => { live++; peak = Math.max(peak, live); await sleep(5); live--; });
  assert.strictEqual(peak, 4, `default peak ${peak}`);
  // The first error rejects, and nothing new starts after it.
  const calls = [];
  await assert.rejects(
    fetchAll(ids, async (i) => {
      calls.push(i);
      await sleep(10);
      if (i === 1) throw new Error("boom 1");
      return i;
    }, { concurrency: 2 }),
    /boom 1/
  );
  await sleep(60);
  assert.ok(calls.length <= 3, `started ${calls.length} calls after the failure`);
  console.log("hidden ok");
})().catch((e) => { console.error(e); process.exit(1); });
