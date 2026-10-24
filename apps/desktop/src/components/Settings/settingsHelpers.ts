import type {
  AiSettings,
  AiSettingsPatch,
  BlenderStatus,
  ClaudePlanStatus,
  Collection,
  ExportSettings,
  ExportSettingsPatch,
  ImportCache,
  ImportSettings,
  ImportSettingsPatch,
  ToolStatus,
} from '../../types';
import { formatSize } from '../../utils/format';

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

/** The export settings fields of `draft` that differ from `saved`, or null when nothing changed. */
export function exportSettingsPatch(
  saved: ExportSettings,
  draft: ExportSettings
): ExportSettingsPatch | null {
  return changedFields(saved, draft) ?? null;
}

/** The import settings as they would be saved: trimmed, an empty field unset (its default). */
export function cleanImportSettings(draft: ImportSettings): ImportSettings {
  const clean = (v: string | null) => v?.trim() || null;
  return {
    blenderMemoryMax: clean(draft.blenderMemoryMax),
    unrealMemoryMax: clean(draft.unrealMemoryMax),
    unrealEditorRoot: clean(draft.unrealEditorRoot),
    assimpPath: clean(draft.assimpPath),
  };
}

/** The import settings fields of `draft` that differ from `saved`, or null when nothing changed. */
export function importSettingsPatch(
  saved: ImportSettings,
  draft: ImportSettings
): ImportSettingsPatch | null {
  return changedFields(saved, cleanImportSettings(draft)) ?? null;
}

/** Why `value` isn't a memory cap systemd reads (digits, not all zero, then K, M, G or T), or
 * null when it is one or is empty, the default. As the backend's `check_cap`. */
export function capProblem(value: string | null): string | null {
  const v = value?.trim() ?? '';
  if (v === '') return null;
  return /^\d*[1-9]\d*[KMGT]?$/.test(v) ? null : 'Give a size such as 12G (K, M, G or T)';
}

/** Why `value` isn't an absolute path, or null when it is one or is empty, the default. */
export function absoluteProblem(value: string | null): string | null {
  const v = value?.trim() ?? '';
  return v === '' || v.startsWith('/') ? null : 'Give an absolute path, starting with /';
}

/** The first problem with the import settings typed, which keeps them from saving, or null. */
export function importSettingsProblem(draft: ImportSettings): string | null {
  const problems: [string, string | null][] = [
    ["Blender's memory cap", capProblem(draft.blenderMemoryMax)],
    ["The Unreal Editor's memory cap", capProblem(draft.unrealMemoryMax)],
    ["The Unreal Editor's folder", absoluteProblem(draft.unrealEditorRoot)],
    ["assimp's path", absoluteProblem(draft.assimpPath)],
  ];
  const found = problems.find(([, problem]) => problem);
  return found ? `${found[0]}: ${found[1]}` : null;
}

/** What Settings says about a tool: "assimp 6.0 at /usr/bin/assimp", or why there is none. */
export function toolLine(name: string, status: ToolStatus): string {
  if (status.path && status.version) return `${name} ${status.version} at ${status.path}`;
  return status.problem ?? `${name} not found`;
}

/** What Settings says about UnityPy, which the preview engine ships. */
export function unityPyLine(unitypy: boolean | null): string {
  if (unitypy === null) return "Not known yet: the preview engine hasn't started";
  return unitypy
    ? 'UnityPy imports in the preview engine'
    : "UnityPy can't be imported in the preview engine, so binary Unity files can't be read";
}

/** What Settings says about the conversion cache: its size and where it is. */
export function cacheLine(cache: ImportCache): string {
  const size = cache.bytes === 0 ? 'Empty' : formatSize(cache.bytes);
  return `Conversion cache: ${size}, at ${cache.path}`;
}

/** The collections the root collection picker offers: top-level, non-smart ones, by name. */
export function rootCollectionOptions(collections: Collection[]): Collection[] {
  return collections
    .filter((c) => !c.parentId && !c.isSmart)
    .sort((a, b) => a.name.localeCompare(b.name));
}

/** What Settings says about Blender: "Blender 5.2.2 at /usr/bin/blender", or why there is none. */
export function blenderLine(status: BlenderStatus): string {
  if (status.path && status.version) return `Blender ${status.version} at ${status.path}`;
  return status.problem ?? 'Blender not found';
}

/**
 * The Import tools section's Blender line: what the check found, why it failed, or why it hasn't
 * answered. Without Blender's path, which Game export failed to load, nothing is checked.
 */
export function importBlenderLine(
  status: BlenderStatus | null,
  checkError: string | null,
  pathError: string | null
): string {
  if (status) return blenderLine(status);
  if (checkError) return checkError;
  if (pathError) return `Not checked: Blender's path couldn't be loaded (${pathError})`;
  return 'Checking Blender…';
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
