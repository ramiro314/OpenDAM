import type { AiSettings, AiSettingsPatch } from '../../types';

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
