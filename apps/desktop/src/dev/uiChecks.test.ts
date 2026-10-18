import { describe, expect, test } from 'bun:test';
import {
  runUiChecks,
  selectChecks,
  type UiCheck,
  type UiCheckContext,
  type UiCheckResult,
} from './uiChecks';

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

describe('selectChecks', () => {
  const checks: UiCheck[] = ['U1', 'U3', 'U8'].map((id) => ({
    id,
    title: id,
    run: async () => {},
  }));
  const ids = (cs: UiCheck[]) => cs.map((c) => c.id);

  test('null runs every check', () => {
    expect(ids(selectChecks(checks, null))).toEqual(['U1', 'U3', 'U8']);
  });

  test('picks the named checks in registry order', () => {
    expect(ids(selectChecks(checks, 'U8'))).toEqual(['U8']);
    expect(ids(selectChecks(checks, 'U8, U1'))).toEqual(['U1', 'U8']);
  });

  test('an unknown id throws', () => {
    expect(() => selectChecks(checks, 'U8,U9')).toThrow('No UI check is named U9');
  });
});
