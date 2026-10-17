import type {
  AutotagMode,
  AutotagProviderOption,
  AutotagScope,
  AutotagTarget,
  RunSummary,
} from '../types';

export const MODES: { mode: AutotagMode; label: string; description: string }[] = [
  {
    mode: 'existing_only',
    label: 'Existing only',
    description: 'Adds matching tags from your catalog. Never removes a tag.',
  },
  {
    mode: 'prefer_existing',
    label: 'Prefer existing',
    description: 'Also collects new tag names for one review at the end.',
  },
  {
    mode: 'create_freely',
    label: 'Create freely',
    description: 'Also creates and applies new tags without asking.',
  },
];

/** The modes the run engine can do so far. */
const BUILT_MODES: readonly AutotagMode[] = ['existing_only', 'prefer_existing'];

export interface Availability {
  enabled: boolean;
  /** Why the entry point is disabled, when it is */
  reason: string | null;
}

/**
 * Whether an auto-tag entry point can open the run dialog, and why not when it can't.
 * `assetCount` is what the entry point would tag (the selection, the right-clicked assets, a
 * library's assets), and `emptyReason` says why it can't tag nothing.
 */
export function autotagAvailability(
  providers: AutotagProviderOption[] | null,
  assetCount: number,
  emptyReason = 'Select assets to auto-tag'
): Availability {
  if (providers === null) return { enabled: false, reason: 'Checking tagging providers…' };
  if (!providers.some((p) => p.unavailableReason === null)) {
    const reasons = providers.map((p) => `${p.unavailableReason}.`);
    return {
      enabled: false,
      reason: ['No tagging provider is ready.', ...reasons].join(' '),
    };
  }
  if (assetCount === 0) return { enabled: false, reason: emptyReason };
  return { enabled: true, reason: null };
}

/** What the run dialog asks the backend to tag: the chosen assets, or the library with its "All assets" choice. */
export function scopeFor(target: AutotagTarget, allAssets: boolean): AutotagScope {
  return target.kind === 'assets'
    ? { kind: 'assets', assetIds: target.assetIds }
    : { kind: 'library', libraryId: target.libraryId, allAssets };
}

export interface ModeOption {
  mode: AutotagMode;
  label: string;
  description: string;
  enabled: boolean;
  reason: string | null;
}

/** Every mode, enabled only when the provider supports it and the engine can run it. */
export function modeOptions(provider: AutotagProviderOption | null): ModeOption[] {
  return MODES.map(({ mode, label, description }) => {
    let reason: string | null = null;
    if (!provider) reason = 'Pick a provider first';
    else if (!provider.modes.includes(mode))
      reason = `${provider.label} can only match existing tags, not propose new ones`;
    else if (!BUILT_MODES.includes(mode)) reason = 'Coming in a later update';
    return { mode, label, description, enabled: reason === null, reason };
  });
}

export function modeLabel(mode: AutotagMode): string {
  return MODES.find((m) => m.mode === mode)?.label ?? mode;
}

function plural(n: number, word: string): string {
  return `${n.toLocaleString()} ${word}${n === 1 ? '' : 's'}`;
}

/** The status bar's tagging entry, or null when there are no runs. */
export function taggingSummary(runs: RunSummary[]): string | null {
  if (runs.length === 0) return null;
  const active = runs.filter((r) => r.status === 'running' || r.status === 'queued');
  const review = runs.filter(needsReview).length;
  const paused = runs.filter((r) => r.status === 'paused').length;

  const parts: string[] = [];
  if (active.length > 0) {
    const processed = active.reduce((sum, r) => sum + r.done + r.failed, 0);
    const total = active.reduce((sum, r) => sum + r.total, 0);
    parts.push(
      `Tagging ${plural(active.length, 'run')} - ${processed.toLocaleString()}/${total.toLocaleString()}`
    );
  }
  if (review > 0) parts.push(`${plural(review, 'run')} ${review === 1 ? 'needs' : 'need'} review`);
  if (paused > 0) parts.push(`${plural(paused, 'run')} paused`);
  if (parts.length === 0) parts.push(`${plural(runs.length, 'tagging run')} finished`);
  return parts.join(' · ');
}

/** "120 / 400", plus the failed and skipped counts when there are some. */
export function runProgress(run: RunSummary): string {
  const processed = run.done + run.failed;
  const parts = [`${processed.toLocaleString()} / ${run.total.toLocaleString()}`];
  if (run.failed > 0) parts.push(`${run.failed.toLocaleString()} failed`);
  if (run.skipped > 0) parts.push(`${run.skipped.toLocaleString()} skipped`);
  return parts.join(' · ');
}

/** A run whose new tag names wait for review: one that finished with some, or was cancelled with some. */
export function needsReview(run: Pick<RunSummary, 'status' | 'pendingProposals'>): boolean {
  return (
    run.pendingProposals > 0 && (run.status === 'awaiting_review' || run.status === 'cancelled')
  );
}

export interface RunActions {
  pause: boolean;
  resume: boolean;
  cancel: boolean;
  /** "Review (N)" */
  review: boolean;
}

/** The Runs panel buttons a run offers. */
export function runActions(run: Pick<RunSummary, 'status' | 'pendingProposals'>): RunActions {
  const { status } = run;
  return {
    // A queued run is still being created: it can be cancelled, not paused
    pause: status === 'running',
    resume: status === 'paused',
    cancel: status === 'running' || status === 'queued' || status === 'paused',
    review: needsReview(run),
  };
}

/** Dollars, with enough decimals that a fraction of a cent doesn't read as $0.00. */
export function formatCost(usd: number): string {
  if (usd > 0 && usd < 0.01) return `$${usd.toPrecision(2)}`;
  return `$${usd.toFixed(2)}`;
}

/** "4,000 tokens · $0.0021", or null before the run has used any tokens. */
export function runUsage(run: RunSummary): string | null {
  const tokens = run.inputTokens + run.outputTokens;
  if (tokens === 0) return null;
  return `${tokens.toLocaleString()} tokens · ${formatCost(run.costUsd)}`;
}
