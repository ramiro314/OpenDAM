import { describe, expect, test } from 'bun:test';
import type { AutotagProviderOption, RunSummary } from '../types';
import {
  autotagAvailability,
  modeOptions,
  runActions,
  runProgress,
  taggingSummary,
} from './autotag';

const ALL_MODES = ['existing_only', 'prefer_existing', 'create_freely'] as const;

function provider(
  id: string,
  unavailableReason: string | null,
  modes: AutotagProviderOption['modes'] = [...ALL_MODES]
): AutotagProviderOption {
  return { id, label: id[0].toUpperCase() + id.slice(1), modes, unavailableReason };
}

function run(status: RunSummary['status'], done: number, total: number, failed = 0): RunSummary {
  return {
    id: `${status}-${done}-${total}`,
    label: `${total} assets`,
    provider: 'fake',
    providerLabel: 'Fake',
    model: 'fake',
    mode: 'existing_only',
    status,
    pauseReason: null,
    total,
    done,
    failed,
    skipped: 0,
    running: 0,
    queued: total - done - failed,
    inputTokens: 0,
    outputTokens: 0,
    costUsd: 0,
    createdAt: 0,
    finishedAt: null,
  };
}

describe('autotagAvailability', () => {
  test('with nothing configured it is disabled and says why', () => {
    const providers = [
      provider('anthropic', 'Set up Anthropic in Settings'),
      provider('jev', 'Set up Jev in Settings', ['existing_only']),
    ];

    expect(autotagAvailability(providers, 3)).toEqual({
      enabled: false,
      reason: 'No tagging provider is ready. Set up Anthropic in Settings. Set up Jev in Settings.',
    });
  });

  test('with a ready provider it needs a selection', () => {
    const providers = [
      provider('anthropic', 'Set up Anthropic in Settings'),
      provider('fake', null),
    ];

    expect(autotagAvailability(providers, 0)).toEqual({
      enabled: false,
      reason: 'Select assets to auto-tag',
    });
    expect(autotagAvailability(providers, 2)).toEqual({ enabled: true, reason: null });
  });

  test('is disabled while providers load', () => {
    expect(autotagAvailability(null, 2).enabled).toBe(false);
  });
});

describe('modeOptions', () => {
  test('a text-only provider allows only Existing only, with the reason', () => {
    const options = modeOptions(provider('jev', null, ['existing_only']));

    expect(options.map((o) => [o.mode, o.enabled])).toEqual([
      ['existing_only', true],
      ['prefer_existing', false],
      ['create_freely', false],
    ]);
    expect(options[1].reason).toBe('Jev can only match existing tags, not propose new ones');
  });

  test('modes the engine cannot run yet are disabled for every provider', () => {
    const options = modeOptions(provider('fake', null));

    expect(options.map((o) => o.enabled)).toEqual([true, false, false]);
    expect(options[2].reason).toBe('Coming in a later update');
  });

  test('with no provider every mode is disabled', () => {
    expect(modeOptions(null).every((o) => !o.enabled && o.reason)).toBe(true);
  });
});

describe('taggingSummary', () => {
  test('is null with no runs', () => {
    expect(taggingSummary([])).toBeNull();
  });

  test('sums the progress of active runs', () => {
    expect(
      taggingSummary([run('running', 100, 300, 20), run('running', 0, 100), run('done', 5, 5)])
    ).toBe('Tagging 2 runs - 120/400');
    expect(taggingSummary([run('running', 1, 4)])).toBe('Tagging 1 run - 1/4');
  });

  test('names runs that need review or are paused', () => {
    expect(taggingSummary([run('awaiting_review', 5, 5)])).toBe('1 run needs review');
    expect(taggingSummary([run('paused', 1, 5), run('paused', 2, 5), run('running', 0, 2)])).toBe(
      'Tagging 1 run - 0/2 · 2 runs paused'
    );
  });

  test('with only finished runs it still offers the Runs panel', () => {
    expect(taggingSummary([run('done', 5, 5), run('done', 2, 2)])).toBe('2 tagging runs finished');
  });
});

describe('runProgress', () => {
  test('shows processed over total, and failures', () => {
    expect(runProgress(run('running', 3, 10))).toBe('3 / 10');
    expect(runProgress(run('done', 8, 10, 2))).toBe('10 / 10 · 2 failed');
  });

  test('shows what a cancel skipped', () => {
    expect(runProgress({ ...run('cancelled', 3, 10, 1), skipped: 6 })).toBe(
      '4 / 10 · 1 failed · 6 skipped'
    );
  });
});

describe('runActions', () => {
  test('an active run can be paused or cancelled', () => {
    expect(runActions('running')).toEqual({ pause: true, resume: false, cancel: true });
    expect(runActions('queued')).toEqual({ pause: true, resume: false, cancel: true });
  });

  test('a paused run can be resumed or cancelled', () => {
    expect(runActions('paused')).toEqual({ pause: false, resume: true, cancel: true });
  });

  test('a stopped run offers none of them', () => {
    for (const status of ['done', 'cancelled', 'awaiting_review', 'undone'] as const) {
      expect(runActions(status)).toEqual({ pause: false, resume: false, cancel: false });
    }
  });
});
