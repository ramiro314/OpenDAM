import { describe, expect, test } from 'bun:test';
import { runUiChecks, type UiCheckContext, type UiCheckResult } from './uiChecks';

const ctx: UiCheckContext = {
  scroll: () => {
    throw new Error('no DOM in unit tests');
  },
  assert: (condition, message) => {
    if (!condition) throw new Error(message);
  },
  waitFor: async () => {},
  pause: async () => {},
  sleep: async () => {},
};

describe('runUiChecks', () => {
  test('records PASS for a check that resolves and FAIL with the error for one that throws', async () => {
    const report = await runUiChecks(
      [
        { id: 'U1', title: 'passes', run: async () => {} },
        { id: 'U2', title: 'fails', run: async (c) => c.assert(false, 'scrollHeight is 900') },
      ],
      ctx
    );
    expect(report.passed).toBe(1);
    expect(report.failed).toBe(1);
    expect(report.results).toEqual([
      { id: 'U1', title: 'passes', result: 'PASS' },
      { id: 'U2', title: 'fails', result: 'FAIL', detail: 'Error: scrollHeight is 900' },
    ]);
  });

  test('a failing check does not stop the ones after it', async () => {
    const report = await runUiChecks(
      [
        { id: 'U1', title: 'throws', run: async () => Promise.reject(new Error('boom')) },
        { id: 'U2', title: 'passes', run: async () => {} },
      ],
      ctx
    );
    expect(report.results.map((r) => r.result)).toEqual(['FAIL', 'PASS']);
  });

  test('runs before ahead of each check and hands each result to after', async () => {
    const events: string[] = [];
    await runUiChecks(
      [
        { id: 'U1', title: 'a', run: async () => void events.push('run U1') },
        { id: 'U2', title: 'b', run: async () => void events.push('run U2') },
      ],
      ctx,
      {
        before: async () => void events.push('before'),
        after: async (r: UiCheckResult) => void events.push(`after ${r.id} ${r.result}`),
      }
    );
    expect(events).toEqual([
      'before',
      'run U1',
      'after U1 PASS',
      'before',
      'run U2',
      'after U2 PASS',
    ]);
  });

  test('no checks gives an empty report', async () => {
    expect(await runUiChecks([], ctx)).toEqual({ passed: 0, failed: 0, results: [] });
  });
});
