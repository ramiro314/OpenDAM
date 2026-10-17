import { describe, expect, test } from 'bun:test';
import type { AutotagProviderOption, ReviewProposal, RunEstimate, RunSummary } from '../types';
import {
  autotagAvailability,
  clearedWithFinished,
  estimateSkips,
  estimateSummary,
  formatCost,
  modeOptions,
  runActions,
  runProgress,
  runUsage,
  scopeFor,
  suggestionChips,
  suggestTagsAvailability,
  taggingSummary,
} from './autotag';

const ALL_MODES = ['existing_only', 'prefer_existing', 'create_freely'] as const;

function provider(
  id: string,
  unavailableReason: string | null,
  modes: AutotagProviderOption['modes'] = [...ALL_MODES]
): AutotagProviderOption {
  return { id, label: id[0].toUpperCase() + id.slice(1), access: null, modes, unavailableReason };
}

function run(status: RunSummary['status'], done: number, total: number, failed = 0): RunSummary {
  return {
    id: `${status}-${done}-${total}`,
    label: `${total} assets`,
    provider: 'fake',
    providerLabel: 'Fake',
    access: null,
    model: 'fake',
    mode: 'existing_only',
    status,
    pauseReason: null,
    notice: null,
    inline: false,
    total,
    done,
    failed,
    skipped: 0,
    running: 0,
    queued: total - done - failed,
    pendingProposals: status === 'awaiting_review' ? 2 : 0,
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

  test('the grid menu item acts on at least the clicked asset', () => {
    const unconfigured = [provider('anthropic', 'Set up Anthropic in Settings')];

    expect(autotagAvailability(unconfigured, 1)).toEqual({
      enabled: false,
      reason: 'No tagging provider is ready. Set up Anthropic in Settings.',
    });
    expect(autotagAvailability([provider('fake', null)], 1)).toEqual({
      enabled: true,
      reason: null,
    });
  });

  test('the sidebar library item says why a library cannot be tagged', () => {
    const unconfigured = [provider('anthropic', 'Set up Anthropic in Settings')];
    const ready = [provider('fake', null)];
    const empty = 'This library has no assets';

    expect(autotagAvailability(unconfigured, 40, empty).reason).toBe(
      'No tagging provider is ready. Set up Anthropic in Settings.'
    );
    expect(autotagAvailability(ready, 0, empty)).toEqual({ enabled: false, reason: empty });
    expect(autotagAvailability(ready, 40, empty)).toEqual({ enabled: true, reason: null });
  });

  test('is disabled while providers load', () => {
    expect(autotagAvailability(null, 2).enabled).toBe(false);
  });
});

describe('scopeFor', () => {
  test('an assets target sends its asset ids, whatever the checkbox says', () => {
    expect(scopeFor({ kind: 'assets', assetIds: ['a1', 'a2'] }, true)).toEqual({
      kind: 'assets',
      assetIds: ['a1', 'a2'],
    });
  });

  test('a library target sends its id and the All assets choice', () => {
    const library = { kind: 'library', libraryId: 'lib-1', libraryName: 'Kenney' } as const;

    expect(scopeFor(library, false)).toEqual({
      kind: 'library',
      libraryId: 'lib-1',
      allAssets: false,
    });
    expect(scopeFor(library, true)).toEqual({
      kind: 'library',
      libraryId: 'lib-1',
      allAssets: true,
    });
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

  test('a provider that can propose offers every mode, Create freely included', () => {
    const options = modeOptions(provider('fake', null));

    expect(options.map((o) => [o.mode, o.enabled, o.reason])).toEqual([
      ['existing_only', true, null],
      ['prefer_existing', true, null],
      ['create_freely', true, null],
    ]);
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
    expect(
      taggingSummary([
        run('awaiting_review', 5, 5),
        { ...run('cancelled', 2, 5), pendingProposals: 1 },
        run('cancelled', 1, 5),
      ])
    ).toBe('2 runs need review');
    expect(taggingSummary([run('paused', 1, 5), run('paused', 2, 5), run('running', 0, 2)])).toBe(
      'Tagging 1 run - 0/2 · 2 runs paused'
    );
  });

  test('names runs partly undone, which wait for Finish undo', () => {
    expect(taggingSummary([run('undo_partial', 5, 5), run('undone', 2, 2)])).toBe(
      '1 run partly undone'
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
  const none = {
    pause: false,
    resume: false,
    cancel: false,
    review: false,
    undo: false,
    finishUndo: false,
    retryFailed: false,
    clear: false,
  };

  test('a running run can be paused or cancelled, not cleared', () => {
    expect(runActions(run('running', 1, 4))).toEqual({ ...none, pause: true, cancel: true });
  });

  test('a run still being created can be cancelled but not paused', () => {
    expect(runActions(run('queued', 0, 4))).toEqual({ ...none, cancel: true });
  });

  test('a paused run can be resumed, cancelled or undone, not cleared', () => {
    expect(runActions(run('paused', 1, 4))).toEqual({
      ...none,
      resume: true,
      cancel: true,
      undo: true,
    });
  });

  test('a stopped run offers Undo and Clear, and an undone one only Clear', () => {
    for (const status of ['done', 'cancelled'] as const) {
      expect(runActions(run(status, 4, 4))).toEqual({ ...none, undo: true, clear: true });
    }
    expect(runActions(run('undone', 4, 4))).toEqual({ ...none, clear: true });
  });

  test('a run being undone offers nothing', () => {
    expect(runActions(run('undoing', 4, 4))).toEqual(none);
  });

  test('a partly undone run offers Finish undo and Clear', () => {
    expect(runActions(run('undo_partial', 4, 4))).toEqual({
      ...none,
      finishUndo: true,
      clear: true,
    });
  });

  test('a run waiting for review, or cancelled with proposals left, offers Review', () => {
    expect(runActions(run('awaiting_review', 4, 4))).toEqual({
      ...none,
      review: true,
      undo: true,
      clear: true,
    });
    expect(runActions({ ...run('cancelled', 2, 4), pendingProposals: 3 })).toEqual({
      ...none,
      review: true,
      undo: true,
      clear: true,
    });
    expect(runActions({ ...run('paused', 2, 4), pendingProposals: 3 }).review).toBe(false);
  });

  test('a run with failed assets offers Retry failed, unless it is being or was undone', () => {
    for (const status of ['running', 'paused', 'awaiting_review', 'done', 'cancelled'] as const) {
      expect(runActions(run(status, 2, 4, 2)).retryFailed).toBe(true);
    }
    for (const status of ['undoing', 'undo_partial', 'undone'] as const) {
      expect(runActions(run(status, 2, 4, 2)).retryFailed).toBe(false);
    }
    expect(runActions(run('done', 4, 4)).retryFailed).toBe(false);
  });
});

describe('clearedWithFinished', () => {
  test('takes done, cancelled and undone runs with nothing left to review', () => {
    for (const status of ['done', 'cancelled', 'undone'] as const) {
      expect(clearedWithFinished(run(status, 4, 4))).toBe(true);
    }
    for (const status of [
      'queued',
      'running',
      'paused',
      'undoing',
      'awaiting_review',
      'undo_partial',
    ] as const) {
      expect(clearedWithFinished(run(status, 4, 4))).toBe(false);
    }
    expect(clearedWithFinished({ ...run('cancelled', 2, 4), pendingProposals: 1 })).toBe(false);
  });
});

describe('formatCost', () => {
  test('keeps a fraction of a cent visible', () => {
    expect(formatCost(0)).toBe('$0.00');
    expect(formatCost(0.0021)).toBe('$0.0021');
    expect(formatCost(0.000184)).toBe('$0.00018');
    expect(formatCost(0.01)).toBe('$0.01');
    expect(formatCost(12.345)).toBe('$12.35');
  });
});

describe('runUsage', () => {
  test('shows tokens and the running cost once the run has used any', () => {
    expect(runUsage(run('running', 0, 10))).toBeNull();
    const used = { ...run('running', 2, 10), inputTokens: 3500, outputTokens: 80, costUsd: 0.0043 };
    expect(runUsage(used)).toBe(`${(3580).toLocaleString()} tokens · $0.0043`);
  });

  test('a Claude-plan run shows its tokens on your plan, with no cost', () => {
    const plan = {
      ...run('running', 2, 10),
      access: 'claude_plan' as const,
      inputTokens: 1452,
      outputTokens: 242,
    };
    expect(runUsage(plan)).toBe(`${(1694).toLocaleString()} tokens · on your plan`);
  });
});

describe('suggestTagsAvailability', () => {
  const anthropicPrefer = { provider: 'anthropic', mode: 'prefer_existing' } as const;

  test('with no provider configured the button is disabled and says why', () => {
    const providers = [
      provider('anthropic', 'Set up Anthropic in Settings'),
      provider('jev', 'Set up Jev in Settings', ['existing_only']),
    ];
    const reason =
      'No tagging provider is ready. Set up Anthropic in Settings. Set up Jev in Settings.';

    expect(suggestTagsAvailability(providers, anthropicPrefer)).toEqual({
      enabled: false,
      reason,
      tooltip: reason,
    });
    expect(suggestTagsAvailability(null, anthropicPrefer).enabled).toBe(false);
  });

  test('the tooltip names the last-used provider and mode', () => {
    const providers = [provider('anthropic', null), provider('jev', null, ['existing_only'])];

    expect(suggestTagsAvailability(providers, anthropicPrefer)).toEqual({
      enabled: true,
      reason: null,
      tooltip: 'Suggest tags with Anthropic, Prefer existing',
    });
    expect(
      suggestTagsAvailability(providers, { provider: 'jev', mode: 'existing_only' }).tooltip
    ).toBe('Suggest tags with Jev, Existing only');
  });

  test('it waits for the last-used provider and mode to load', () => {
    expect(suggestTagsAvailability([provider('anthropic', null)], null)).toEqual({
      enabled: false,
      reason: 'Checking tagging providers…',
      tooltip: 'Checking tagging providers…',
    });
  });

  test('a last-used provider that is not ready disables it, even with another ready', () => {
    const providers = [
      provider('anthropic', 'Set up Anthropic in Settings'),
      provider('jev', null, ['existing_only']),
    ];

    const availability = suggestTagsAvailability(providers, anthropicPrefer);

    expect(availability.enabled).toBe(false);
    expect(availability.reason).toBe(
      'Suggest tags uses the provider of your last run, Anthropic: Set up Anthropic in Settings. Start a run from Auto-tag… with another provider to switch.'
    );
  });

  test('a last-used provider missing from this session disables it', () => {
    const availability = suggestTagsAvailability([provider('anthropic', null)], {
      provider: 'fake',
      mode: 'prefer_existing',
    });

    expect(availability.enabled).toBe(false);
    expect(availability.reason).toContain("fake, which isn't available");
  });

  test('a mode the provider cannot do disables it with the reason', () => {
    const availability = suggestTagsAvailability([provider('jev', null, ['existing_only'])], {
      provider: 'jev',
      mode: 'prefer_existing',
    });

    expect(availability).toEqual({
      enabled: false,
      reason:
        'Suggest tags uses the mode of your last run, Prefer existing: Jev can only match existing tags, not propose new ones.',
      tooltip:
        'Suggest tags uses the mode of your last run, Prefer existing: Jev can only match existing tags, not propose new ones.',
    });
  });
});

describe('suggestionChips', () => {
  const proposal = (id: string, name: string, parentId: string | null = null): ReviewProposal => ({
    id,
    name,
    parentId,
    assets: [{ assetId: 'a1', fileName: 'a1.png', thumbnailPath: null, included: true }],
  });

  test('one chip per name across runs, as the newest run proposed it', () => {
    const newer = { run: { ...run('awaiting_review', 1, 1), id: 'r2' }, error: null };
    const older = { run: { ...run('awaiting_review', 1, 1), id: 'r1' }, error: null };

    const chips = suggestionChips([
      { ...newer, proposals: [proposal('p3', 'Moss', 't-nature'), proposal('p4', 'Large')] },
      { ...older, proposals: [proposal('p1', ' moss '), proposal('p2', 'Lichen')] },
    ]);

    expect(chips).toEqual([
      {
        name: 'Moss',
        parentId: 't-nature',
        proposals: [
          { runId: 'r2', proposalId: 'p3', assetIds: ['a1'] },
          { runId: 'r1', proposalId: 'p1', assetIds: ['a1'] },
        ],
      },
      {
        name: 'Large',
        parentId: null,
        proposals: [{ runId: 'r2', proposalId: 'p4', assetIds: ['a1'] }],
      },
      {
        name: 'Lichen',
        parentId: null,
        proposals: [{ runId: 'r1', proposalId: 'p2', assetIds: ['a1'] }],
      },
    ]);
  });

  test("a chip's assets are its proposals' own, whatever asset is shown", () => {
    const other = {
      ...proposal('p9', 'Sand'),
      assets: [{ ...proposal('x', 'x').assets[0], assetId: 'a9' }],
    };

    const [chip] = suggestionChips([
      { run: { ...run('awaiting_review', 1, 1), id: 'r9' }, error: null, proposals: [other] },
    ]);

    expect(chip.proposals).toEqual([{ runId: 'r9', proposalId: 'p9', assetIds: ['a9'] }]);
  });

  test('no proposals, no chips', () => {
    expect(suggestionChips([])).toEqual([]);
    expect(suggestionChips([{ run: run('done', 1, 1), error: null, proposals: [] }])).toEqual([]);
  });
});

describe('estimateSummary', () => {
  const estimate = (e: RunEstimate['estimate'], assets = 120): RunEstimate => ({
    assets,
    skipped: 0,
    missing: 0,
    estimate: e,
  });

  test('shows the asset count and the cost with an API key', () => {
    expect(
      estimateSummary(estimate({ kind: 'cost', usd: 0.4216, model: 'claude-sonnet-5-5' }))
    ).toBe('120 assets · about $0.42');
    expect(estimateSummary(estimate({ kind: 'cost', usd: 0.0021, model: 'jev-latest' }, 1))).toBe(
      '1 asset · about $0.0021'
    );
  });

  test('shows a request count on the Claude plan', () => {
    expect(estimateSummary(estimate({ kind: 'requests', count: 1200 }, 1200))).toBe(
      `${(1200).toLocaleString()} assets · ${(1200).toLocaleString()} requests on your Claude plan`
    );
    expect(estimateSummary(estimate({ kind: 'requests', count: 1 }, 1))).toBe(
      '1 asset · 1 request on your Claude plan'
    );
  });

  test('says when the model has no price', () => {
    expect(estimateSummary(estimate({ kind: 'cost', usd: null, model: 'my-model' }))).toBe(
      '120 assets · cost unknown: no price for my-model'
    );
  });
});

describe('estimateSkips', () => {
  const skips = (skipped: number, missing: number) =>
    estimateSkips({ assets: 5, skipped, missing, estimate: { kind: 'requests', count: 5 } });

  test('names each reason an asset is left out', () => {
    expect(skips(0, 0)).toEqual([]);
    expect(skips(1, 0)).toEqual(['Skips 1 asset already queued in another run']);
    expect(skips(12, 3)).toEqual([
      'Skips 12 assets already queued in another run',
      'Skips 3 assets not in an open library',
    ]);
  });
});
