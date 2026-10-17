import type { AiSettings, AiSettingsPatch, ClaudePlanStatus } from '../../types';

/** The Claude models Settings offers by name. Any other id is a custom model. */
export const ANTHROPIC_MODELS = [
  { id: 'claude-sonnet-5-5', label: 'Sonnet 5.5 (default)' },
  { id: 'claude-opus-5-5', label: 'Opus 5.5' },
  { id: 'claude-haiku-5-5', label: 'Haiku 5.5' },
] as const;

export function isKnownModel(model: string): boolean {
  return ANTHROPIC_MODELS.some((m) => m.id === model);
}

function changedFields<T extends object>(saved: T, draft: T): Partial<T> | undefined {
  const keys = (Object.keys(draft) as (keyof T)[]).filter((k) => draft[k] !== saved[k]);
  if (keys.length === 0) return undefined;
  return Object.fromEntries(keys.map((k) => [k, draft[k]])) as Partial<T>;
}

/** The fields of `draft` that differ from `saved`, or null when nothing changed. */
export function settingsPatch(saved: AiSettings, draft: AiSettings): AiSettingsPatch | null {
  const anthropic = changedFields(saved.anthropic, draft.anthropic);
  const jev = changedFields(saved.jev, draft.jev);
  if (!anthropic && !jev) return null;
  return { ...(anthropic && { anthropic }), ...(jev && { jev }) };
}

/** What Settings says about Claude-plan access: where `claude` is, and which login it uses. */
export function claudePlanLines(status: ClaudePlanStatus): { found: string; login: string } {
  const found = status.path
    ? `claude ${status.version ?? '(version unknown)'} at ${status.path}`
    : 'claude not found on PATH';
  let login = 'Logged out';
  if (status.loggedIn && status.authMethod === 'claude.ai') {
    const plan = status.subscriptionType ? `, ${status.subscriptionType} plan` : '';
    login = `Logged in with your Claude account (claude.ai${plan})`;
  } else if (status.loggedIn) {
    login = `Logged in with ${status.authMethod ?? 'an unknown method'}, not your Claude plan`;
  } else if (!status.path) {
    login = 'Not checked';
  }
  return { found, login };
}

/** "ANTHROPIC_API_KEY is set for the app, but claude never gets it", or null when none is set. */
export function ignoredOverridesNote(names: string[]): string | null {
  if (names.length === 0) return null;
  const list = names.join(', ');
  return names.length === 1
    ? `${list} is set for the app, but claude never gets it`
    : `${list} are set for the app, but claude never gets them`;
}

/** The number typed in a settings field, or null while it is empty, partial or not allowed. */
export function parseNumberInput(raw: string, integer: boolean): number | null {
  if (raw.trim() === '') return null;
  const n = Number(raw);
  if (!Number.isFinite(n) || (integer && !Number.isInteger(n))) return null;
  return n;
}
