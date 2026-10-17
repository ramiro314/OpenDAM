import type {
  AutotagLastUsed,
  AutotagMode,
  AutotagProviderOption,
  AutotagScope,
  AutotagSuggestion,
  AutotagTarget,
  RunEstimate,
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

export interface SuggestTagsAvailability extends Availability {
  /** The button's tooltip: the provider and mode it uses, or why it is disabled */
  tooltip: string;
}

/**
 * Whether "Suggest tags" can tag the asset with the last-used provider and mode (D32), and the
 * tooltip naming them, or saying why it can't.
 */
export function suggestTagsAvailability(
  providers: AutotagProviderOption[] | null,
  lastUsed: AutotagLastUsed | null
): SuggestTagsAvailability {
  const disabled = (reason: string) => ({ enabled: false, reason, tooltip: reason });
  const base = autotagAvailability(providers, 1);
  if (!base.enabled) return disabled(base.reason!);
  if (lastUsed === null) return disabled('Checking tagging providers…');
  const provider = providers!.find((p) => p.id === lastUsed.provider);
  if (!provider) {
    return disabled(
      `Suggest tags uses the provider of your last run, ${lastUsed.provider}, which isn't available. Start a run from Auto-tag… with another provider to switch.`
    );
  }
  if (provider.unavailableReason !== null) {
    return disabled(
      `Suggest tags uses the provider of your last run, ${provider.label}: ${provider.unavailableReason}. Start a run from Auto-tag… with another provider to switch.`
    );
  }
  const mode = modeOptions(provider).find((m) => m.mode === lastUsed.mode);
  if (!mode?.enabled) {
    return disabled(
      `Suggest tags uses the mode of your last run, ${modeLabel(lastUsed.mode)}: ${mode?.reason ?? 'unknown mode'}.`
    );
  }
  return {
    enabled: true,
    reason: null,
    tooltip: `Suggest tags with ${provider.label}, ${mode.label}`,
  };
}

/** A suggested new tag name in the Tags tab, standing for every pending proposal of that name */
export interface SuggestionChip {
  name: string;
  parentId: string | null;
  /**
   * Each run's proposal of this name, with the assets it is for; a decision on the chip applies
   * to all of them
   */
  proposals: { runId: string; proposalId: string; assetIds: string[] }[];
}

/**
 * The chips for an asset's "Suggest tags" runs, newest run first. Two runs proposing the same
 * name (case and spacing aside) give one chip, named and placed as the newest proposed it.
 */
export function suggestionChips(suggestions: AutotagSuggestion[]): SuggestionChip[] {
  const chips = new Map<string, SuggestionChip>();
  for (const { run, proposals } of suggestions) {
    for (const p of proposals) {
      const key = p.name.trim().replace(/\s+/g, ' ').toLowerCase();
      const chip = chips.get(key) ?? { name: p.name, parentId: p.parentId, proposals: [] };
      chip.proposals.push({
        runId: run.id,
        proposalId: p.id,
        assetIds: p.assets.map((a) => a.assetId),
      });
      chips.set(key, chip);
    }
  }
  return [...chips.values()];
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

/** Every mode, enabled only when the provider supports it. */
export function modeOptions(provider: AutotagProviderOption | null): ModeOption[] {
  return MODES.map(({ mode, label, description }) => {
    let reason: string | null = null;
    if (!provider) reason = 'Pick a provider first';
    else if (!provider.modes.includes(mode))
      reason = `${provider.label} can only match existing tags, not propose new ones`;
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

/**
 * "4,000 tokens · $0.0021", or "4,000 tokens · on your plan" for a Claude-plan run, or null
 * before the run has used any tokens.
 */
export function runUsage(run: RunSummary): string | null {
  const tokens = run.inputTokens + run.outputTokens;
  if (tokens === 0) return null;
  const cost = run.access === 'claude_plan' ? 'on your plan' : formatCost(run.costUsd);
  return `${tokens.toLocaleString()} tokens · ${cost}`;
}

/**
 * The run dialog's estimate: "120 assets · about $0.42", or "120 assets · 120 requests on your
 * Claude plan".
 */
export function estimateSummary({ assets, estimate }: RunEstimate): string {
  let usage: string;
  if (estimate.kind === 'requests') {
    usage = `${plural(estimate.count, 'request')} on your Claude plan`;
  } else if (estimate.usd === null) {
    usage = `cost unknown: no price for ${estimate.model ?? 'this model'}`;
  } else {
    usage = `about ${formatCost(estimate.usd)}`;
  }
  return `${plural(assets, 'asset')} · ${usage}`;
}

/** Why the run leaves assets out, one line per reason. */
export function estimateSkips({ skipped, missing }: RunEstimate): string[] {
  const lines = [];
  if (skipped > 0) {
    lines.push(`Skips ${plural(skipped, 'asset')} already queued in another run`);
  }
  if (missing > 0) lines.push(`Skips ${plural(missing, 'asset')} not in an open library`);
  return lines;
}
