import type { AutotagMode, AutotagProviderOption, RunSummary } from '../types';

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
const BUILT_MODES: readonly AutotagMode[] = ['existing_only'];

export interface Availability {
  enabled: boolean;
  /** Why the entry point is disabled, when it is */
  reason: string | null;
}

/** Whether an auto-tag entry point can open the run dialog, and why not when it can't. */
export function autotagAvailability(
  providers: AutotagProviderOption[] | null,
  assetCount: number
): Availability {
  if (providers === null) return { enabled: false, reason: 'Checking tagging providers…' };
  if (!providers.some((p) => p.unavailableReason === null)) {
    const reasons = providers.map((p) => `${p.unavailableReason}.`);
    return {
      enabled: false,
      reason: ['No tagging provider is ready.', ...reasons].join(' '),
    };
  }
  if (assetCount === 0) return { enabled: false, reason: 'Select assets to auto-tag' };
  return { enabled: true, reason: null };
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
  const review = runs.filter((r) => r.status === 'awaiting_review').length;
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

/** "120 / 400", plus the failed count when there is one. */
export function runProgress(run: RunSummary): string {
  const processed = run.done + run.failed;
  const text = `${processed.toLocaleString()} / ${run.total.toLocaleString()}`;
  return run.failed > 0 ? `${text} · ${run.failed.toLocaleString()} failed` : text;
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
