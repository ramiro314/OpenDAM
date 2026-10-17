// Pure helpers for the scroll benchmark's report.

/** Nearest-rank percentile of `values` (any order), or 0 when empty. */
export function percentile(values: number[], p: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.ceil((p / 100) * sorted.length);
  return sorted[Math.min(sorted.length, Math.max(1, rank)) - 1];
}

const round1 = (n: number) => Math.round(n * 10) / 10;

export interface FrameStats {
  frames: number;
  p50Ms: number;
  p95Ms: number;
  maxMs: number;
  over50Ms: number;
  over100Ms: number;
}

/** Summarizes rAF frame intervals in milliseconds. */
export function frameStats(intervals: number[]): FrameStats {
  return {
    frames: intervals.length,
    p50Ms: round1(percentile(intervals, 50)),
    p95Ms: round1(percentile(intervals, 95)),
    maxMs: round1(intervals.length ? Math.max(...intervals) : 0),
    over50Ms: intervals.filter((ms) => ms > 50).length,
    over100Ms: intervals.filter((ms) => ms > 100).length,
  };
}

export interface TimeStats {
  count: number;
  p50Ms: number;
  p95Ms: number;
  maxMs: number;
}

/** Summarizes durations in milliseconds, such as jump time-to-ready. */
export function timeStats(durations: number[]): TimeStats {
  return {
    count: durations.length,
    p50Ms: round1(percentile(durations, 50)),
    p95Ms: round1(percentile(durations, 95)),
    maxMs: round1(durations.length ? Math.max(...durations) : 0),
  };
}

/** Min and max of `samples`, or nulls when there are none. */
export function minMax(samples: number[]): { min: number | null; max: number | null } {
  if (samples.length === 0) return { min: null, max: null };
  return { min: Math.min(...samples), max: Math.max(...samples) };
}

const MB = 1024 * 1024;

/** RSS samples in bytes as start, end and max in MB. */
export function rssStats(bytes: number[]): {
  startMB: number | null;
  endMB: number | null;
  maxMB: number | null;
} {
  if (bytes.length === 0) return { startMB: null, endMB: null, maxMB: null };
  return {
    startMB: round1(bytes[0] / MB),
    endMB: round1(bytes[bytes.length - 1] / MB),
    maxMB: round1(Math.max(...bytes) / MB),
  };
}

/** A seeded PRNG (mulberry32) returning floats in [0, 1), so jump targets repeat across runs. */
export function seededRandom(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
