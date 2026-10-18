import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { onDevicePixelRatioChange } from './devicePixelRatio';

/** A stand-in for the browser's `matchMedia`, recording each query and its change listeners. */
class FakeQuery {
  listeners = new Set<() => void>();
  constructor(readonly media: string) {}
  addEventListener(_: string, l: () => void) {
    this.listeners.add(l);
  }
  removeEventListener(_: string, l: () => void) {
    this.listeners.delete(l);
  }
}

const realWindow = globalThis.window;
let queries: FakeQuery[];
let win: { devicePixelRatio: number; matchMedia: (media: string) => FakeQuery };

/** Moves to `ratio`, firing the armed query's change as the browser would. */
function changeRatio(ratio: number) {
  win.devicePixelRatio = ratio;
  const armed = queries.filter((q) => q.listeners.size > 0);
  for (const q of armed) for (const l of q.listeners) l();
}

beforeEach(() => {
  queries = [];
  win = {
    devicePixelRatio: 1,
    matchMedia: (media) => {
      const q = new FakeQuery(media);
      queries.push(q);
      return q;
    },
  };
  (globalThis as { window: unknown }).window = win;
});

afterEach(() => {
  (globalThis as { window: unknown }).window = realWindow;
});

describe('onDevicePixelRatioChange', () => {
  test('calls every listener on each change, re-arming at the new ratio', () => {
    const calls: string[] = [];
    const offA = onDevicePixelRatioChange(() => calls.push('a'));
    const offB = onDevicePixelRatioChange(() => calls.push('b'));
    expect(queries.map((q) => q.media)).toEqual(['(resolution: 1dppx)']);
    changeRatio(2);
    expect(calls).toEqual(['a', 'b']);
    expect(queries.at(-1)!.media).toBe('(resolution: 2dppx)');
    changeRatio(1.25);
    expect(calls).toEqual(['a', 'b', 'a', 'b']);
    expect(queries.at(-1)!.media).toBe('(resolution: 1.25dppx)');
    // only the latest query is listened to
    expect(queries.filter((q) => q.listeners.size > 0)).toHaveLength(1);
    offA();
    offB();
  });

  test('an unsubscribed listener is not called, and the last one out stops listening', () => {
    const calls: string[] = [];
    const offA = onDevicePixelRatioChange(() => calls.push('a'));
    const offB = onDevicePixelRatioChange(() => calls.push('b'));
    offA();
    changeRatio(2);
    expect(calls).toEqual(['b']);
    offB();
    expect(queries.every((q) => q.listeners.size === 0)).toBe(true);
    changeRatio(1);
    expect(calls).toEqual(['b']);
  });
});
