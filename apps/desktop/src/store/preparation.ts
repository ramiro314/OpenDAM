import { create } from 'zustand';
import { api } from '../api';
import type {
  ImportPackSteps,
  ImportPackSummary,
  ImportRun,
  ImportStepRecord,
  ImportSummary,
} from '../types';
import { cleanTarget, isLive, upsertRun, useImportStore } from './imports';

// An imported pack's Preparation (plan pack-import, UI): its steps with their toggles and
// options, its target, and Rebuild. The pack's details and the Imports view open the same
// editor, kept here by pack id.

/** What a step edit or a Rebuild answers while a run for the pack is live (`engine::BUSY`) */
export const BUSY = 'An import run is building this pack';

/** What a step edit answers once the steps moved on since they were read (`engine::STALE`) */
export const STALE = "The pack's steps changed since they were read";

/** The import and pack whose target is folder `path` of library `libraryId`, or null */
export function preparationPack(
  imports: Record<string, ImportSummary[]>,
  libraryId: string,
  path: string
): { original: ImportSummary; pack: ImportPackSummary } | null {
  for (const original of imports[libraryId] ?? []) {
    const pack = original.packs.find((p) => p.target === path);
    if (pack) return { original, pack };
  }
  return null;
}

/** The pack's latest run, `runs` being newest first */
export function latestRun(runs: ImportRun[], libraryId: string, packId: string): ImportRun | null {
  return (
    runs.find((r) => r.libraryId === libraryId && r.packs.some((p) => p.packId === packId)) ?? null
  );
}

/** Why the pack's steps can't be edited nor the pack rebuilt now, or null */
export function busyReason(runs: ImportRun[], libraryId: string, packId: string): string | null {
  const live = runs.some(
    (r) => r.libraryId === libraryId && isLive(r) && r.packs.some((p) => p.packId === packId)
  );
  return live ? BUSY : null;
}

// ── Steps ─────────────────────────────────────────────────────────────────

/** A source path in the original, nested packages joined by ` > ` */
function sourceText(source: unknown): string {
  return Array.isArray(source) ? source.join(' > ') : String(source ?? '');
}

/** What a step does, in a line */
export function stepLabel(step: ImportStepRecord): string {
  const o = step.options ?? {};
  const subpath = typeof o.subpath === 'string' ? o.subpath : null;
  switch (step.kind) {
    case 'extract_unitypackage':
      if (typeof o.variant === 'string')
        return `Extract ${o.variant}${subpath ? `, folder ${subpath}` : ''}`;
      return subpath ? `Extract the package's ${subpath}` : 'Extract the package';
    case 'extract_zip':
      return subpath ? `Extract ${subpath} from the zip` : 'Extract the zip';
    case 'flatten':
      return 'Merge wrapper folders into the pack root';
    case 'hoist':
      return 'Move a folder up to the pack root';
    case 'rename':
      return `Rename ${sourceText(o.source)}`;
    case 'exclude':
      return `Leave out ${sourceText(o.source)}`;
    case 'unity_glb':
      return 'Convert models to textured GLB';
    case 'metadata':
      return `Write datapackage.json${typeof o.preset === 'string' ? ` (${o.preset})` : ''}`;
    default:
      return step.kind;
  }
}

export type OptionField =
  | { key: string; label: string; type: 'text' }
  | { key: string; label: string; type: 'list' }
  | { key: string; label: string; type: 'number'; min: number; max: number }
  | { key: string; label: string; type: 'bool' };

/** The options the editor offers for a step; every other option is kept as it is */
export function optionFields(step: ImportStepRecord): OptionField[] {
  switch (step.kind) {
    case 'hoist':
      return [{ key: 'folder', label: 'Folder', type: 'text' }];
    case 'rename':
      return [{ key: 'name', label: 'New name', type: 'text' }];
    case 'unity_glb':
      return [
        { key: 'max_texture', label: 'Max texture (px)', type: 'number', min: 16, max: 16384 },
        { key: 'force', label: 'Convert even where the folder ships a GLB', type: 'bool' },
      ];
    case 'metadata': {
      const fields: OptionField[] = [
        { key: 'title', label: 'Title', type: 'text' },
        { key: 'version', label: 'Version', type: 'text' },
        { key: 'publisher', label: 'Publisher', type: 'text' },
        { key: 'authors', label: 'Authors', type: 'list' },
      ];
      // A whole license entry (name, title, path) has no field here, and is kept
      const license = step.options?.license;
      if (license === undefined || license === null || typeof license === 'string')
        fields.push({ key: 'license', label: 'License', type: 'text' });
      if (step.options?.preset === 'kenney')
        fields.push({ key: 'lookup_store_page', label: 'Look up store page', type: 'bool' });
      return fields;
    }
    default:
      return [];
  }
}

/** A field's value as its input shows it */
export function fieldValue(step: ImportStepRecord, field: OptionField): string | boolean {
  const value = step.options?.[field.key];
  if (field.type === 'bool') return value === true;
  if (field.type === 'list' && Array.isArray(value)) return value.join(', ');
  return value === undefined || value === null ? '' : String(value);
}

/** Metadata fields that an empty value leaves out, so the build falls back on its default */
const OPTIONAL = new Set(['title', 'version', 'publisher', 'license']);

/** The step as `set_import_steps` takes it: typed text parsed, empty optional fields left out */
export function normalizeStep(step: ImportStepRecord): ImportStepRecord {
  const options: Record<string, unknown> = { ...step.options };
  for (const field of optionFields(step)) {
    const value = options[field.key];
    if (typeof value !== 'string') continue;
    if (field.type === 'number') {
      const n = Number(value.trim());
      options[field.key] = value.trim() && Number.isFinite(n) ? n : value;
    } else if (field.type === 'list') {
      options[field.key] = value
        .split(',')
        .map((v) => v.trim())
        .filter(Boolean);
    } else if (step.kind === 'metadata' && OPTIONAL.has(field.key)) {
      if (value.trim()) options[field.key] = value.trim();
      else delete options[field.key];
    }
  }
  return { ...step, options };
}

/** Why a step's options can't be saved, or null */
export function stepProblem(step: ImportStepRecord): string | null {
  const o = normalizeStep(step).options;
  switch (step.kind) {
    case 'hoist':
      return typeof o.folder === 'string' && o.folder.trim() ? null : 'Name the folder to move up';
    case 'rename': {
      const name = typeof o.name === 'string' ? o.name.trim() : '';
      if (!name) return 'Name the file';
      return name.includes('/') ? 'A new name is a file name, without a folder' : null;
    }
    case 'unity_glb': {
      const max = o.max_texture;
      if (max === undefined) return null;
      return typeof max === 'number' && Number.isInteger(max) && max >= 16 && max <= 16384
        ? null
        : 'The max texture is a whole number from 16 to 16384';
    }
    default:
      return null;
  }
}

/** Why step `step` can't be turned on or off now, or null */
export function toggleProblem(
  step: ImportStepRecord,
  unavailable: Record<string, string>
): string | null {
  if (step.kind.startsWith('extract')) return 'A pack is built from its extract step';
  if (!step.enabled && unavailable[step.kind]) return unavailable[step.kind];
  return null;
}

/** A pack's steps as the editor holds them: as read, and as edited */
export interface StepsDraft {
  recipeRev: number;
  saved: ImportStepRecord[];
  steps: ImportStepRecord[];
  unavailable: Record<string, string>;
}

export function draftFromSteps(read: ImportPackSteps): StepsDraft {
  return {
    recipeRev: read.recipeRev,
    saved: read.steps,
    steps: read.steps,
    unavailable: read.unavailable ?? {},
  };
}

/** The draft's steps as they would be saved */
export function stepsToSave(draft: StepsDraft): ImportStepRecord[] {
  return draft.steps.map(normalizeStep);
}

/** JSON with object keys sorted, as the library's step options come back (serde_json's map) */
function stableJson(value: unknown): string {
  return JSON.stringify(value, (_, v: unknown) =>
    v && typeof v === 'object' && !Array.isArray(v)
      ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
      : v
  );
}

/** Whether the draft holds edits not saved yet */
export function isDirty(draft: StepsDraft): boolean {
  return stableJson(stepsToSave(draft)) !== stableJson(draft.saved);
}

/** Turns step `i` on or off, unless `toggleProblem` refuses it */
export function toggleStep(draft: StepsDraft, i: number, enabled: boolean): StepsDraft {
  const step = draft.steps[i];
  if (!step || step.enabled === enabled || toggleProblem(step, draft.unavailable)) return draft;
  const steps = draft.steps.map((s, j) => (j === i ? { ...s, enabled } : s));
  return { ...draft, steps };
}

/** Sets option `key` of step `i` as its input gives it */
export function setOption(
  draft: StepsDraft,
  i: number,
  key: string,
  value: string | boolean
): StepsDraft {
  const steps = draft.steps.map((s, j) =>
    j === i ? { ...s, options: { ...s.options, [key]: value } } : s
  );
  return { ...draft, steps };
}

/** Why the draft can't be saved, naming the step, or null */
export function draftProblem(draft: StepsDraft): string | null {
  for (const step of draft.steps) {
    const problem = stepProblem(step);
    if (problem) return `${stepLabel(step)}: ${problem}`;
  }
  return null;
}

// ── Target ────────────────────────────────────────────────────────────────

/** Why a pack's target can't be edited, or null: a built pack's folder doesn't move yet */
export function targetLocked(pack: ImportPackSummary): string | null {
  return pack.status === 'built' ? "A built pack's folder can't move yet" : null;
}

export interface TargetDraft {
  /** As typed */
  value: string;
  /** The value `error` was checked for; another one is waiting for its check */
  checked: string;
  /** Why the library refuses `checked` */
  error: string | null;
}

/** Why the typed target can't be moved to, or null; `current` is the pack's target */
export function targetProblem(draft: TargetDraft, current: string): string | null {
  const target = cleanTarget(draft.value);
  if (!target) return 'Name the folder to import into';
  if (target === current) return null;
  if (draft.checked !== draft.value) return 'Checking the target…';
  return draft.error;
}

// ── The store ─────────────────────────────────────────────────────────────

export interface Editor {
  libraryId: string;
  packId: string;
  loading: boolean;
  draft: StepsDraft | null;
  /** Why the last read, save or Rebuild failed */
  error: string | null;
  /** A save, a Rebuild or a target move is in flight */
  saving: boolean;
  target: TargetDraft | null;
}

interface PreparationStore {
  /** Each opened pack's editor, by pack id */
  editors: Record<string, Editor>;
  /** Reads the pack's steps, keeping edits not saved yet */
  open: (libraryId: string, packId: string) => Promise<void>;
  /** Reads every open editor's steps again, but those with edits, for `library:updated` */
  refetch: () => Promise<void>;
  edit: (packId: string, change: (draft: StepsDraft) => StepsDraft) => void;
  /** Puts the steps back as read */
  revert: (packId: string) => void;
  /** Saves the edited steps; true once they are saved, or when there was nothing to save */
  save: (packId: string) => Promise<boolean>;
  /** Saves any edits, then queues a rebuild; true once it is queued */
  rebuild: (packId: string) => Promise<boolean>;
  editTarget: (packId: string, value: string) => void;
  /** Asks the library about the typed target, unless it was asked already */
  checkTarget: (packId: string, current: string) => Promise<void>;
  /** Moves the pack's target to the typed one; true once it moved */
  moveTarget: (packId: string, current: string) => Promise<boolean>;
}

export const usePreparationStore = create<PreparationStore>()((set, get) => {
  /** Each pack's latest read, so an older answer never replaces a newer one */
  const reads = new Map<string, number>();

  const patch = (packId: string, change: Partial<Editor>) =>
    set((s) => {
      const editor = s.editors[packId];
      return editor ? { editors: { ...s.editors, [packId]: { ...editor, ...change } } } : {};
    });

  const busy = (editor: Editor) =>
    busyReason(useImportStore.getState().runs, editor.libraryId, editor.packId);

  /** Reads the pack's steps, replacing the draft unless `keepEdits` and it has edits */
  const read = async (libraryId: string, packId: string, keepEdits: boolean) => {
    const request = (reads.get(packId) ?? 0) + 1;
    reads.set(packId, request);
    patch(packId, { loading: true });
    try {
      const steps = await api.getImportSteps(libraryId, packId);
      if (reads.get(packId) !== request) return;
      const editor = get().editors[packId];
      if (!editor) return;
      const kept = keepEdits && editor.draft && isDirty(editor.draft);
      patch(packId, { loading: false, ...(kept ? {} : { draft: draftFromSteps(steps) }) });
    } catch (err) {
      if (reads.get(packId) === request) patch(packId, { loading: false, error: String(err) });
    }
  };

  return {
    editors: {},

    open: async (libraryId, packId) => {
      const held = get().editors[packId];
      if (!held || held.libraryId !== libraryId) {
        const editor: Editor = {
          libraryId,
          packId,
          loading: true,
          draft: null,
          error: null,
          saving: false,
          target: null,
        };
        set((s) => ({ editors: { ...s.editors, [packId]: editor } }));
      }
      await read(libraryId, packId, true);
    },

    refetch: async () => {
      await Promise.all(Object.values(get().editors).map((e) => read(e.libraryId, e.packId, true)));
    },

    edit: (packId, change) => {
      const editor = get().editors[packId];
      if (!editor?.draft || editor.saving || busy(editor)) return;
      patch(packId, { draft: change(editor.draft), error: null });
    },

    revert: (packId) => {
      const editor = get().editors[packId];
      if (!editor?.draft || editor.saving) return;
      patch(packId, { draft: { ...editor.draft, steps: editor.draft.saved }, error: null });
    },

    save: async (packId) => {
      const editor = get().editors[packId];
      if (!editor?.draft || editor.saving) return false;
      if (!isDirty(editor.draft)) return true;
      const reason = busy(editor) ?? draftProblem(editor.draft);
      if (reason) {
        patch(packId, { error: reason });
        return false;
      }
      const steps = stepsToSave(editor.draft);
      patch(packId, { saving: true, error: null });
      try {
        const recipeRev = await api.setImportSteps(
          editor.libraryId,
          packId,
          steps,
          editor.draft.recipeRev
        );
        const now = get().editors[packId]?.draft ?? editor.draft;
        patch(packId, {
          saving: false,
          draft: { ...now, recipeRev, saved: steps, steps },
        });
        return true;
      } catch (err) {
        const message = String(err);
        patch(packId, { saving: false, error: message });
        // Another edit got there first (an in-app rename's step): show the steps it left
        if (message.includes(STALE)) {
          await read(editor.libraryId, packId, false);
          patch(packId, {
            error: `${STALE}, by a rename or delete in the pack: they are shown again`,
          });
        }
        return false;
      }
    },

    rebuild: async (packId) => {
      const editor = get().editors[packId];
      if (!editor?.draft || editor.saving) return false;
      const reason = busy(editor);
      if (reason) {
        patch(packId, { error: reason });
        return false;
      }
      if (!(await get().save(packId))) return false;
      patch(packId, { saving: true, error: null });
      try {
        const run = await api.rebuildImportPack(editor.libraryId, packId);
        useImportStore.setState((s) => ({ runs: upsertRun(s.runs, run) }));
        patch(packId, { saving: false });
        return true;
      } catch (err) {
        patch(packId, { saving: false, error: String(err) });
        return false;
      }
    },

    editTarget: (packId, value) => {
      const editor = get().editors[packId];
      if (!editor || editor.saving) return;
      patch(packId, {
        target: {
          value,
          checked: editor.target?.checked ?? '',
          error: editor.target?.error ?? null,
        },
        error: null,
      });
    },

    checkTarget: async (packId, current) => {
      const editor = get().editors[packId];
      const typed = editor?.target;
      if (!editor || !typed || typed.checked === typed.value) return;
      const value = typed.value;
      let error: string | null = null;
      const target = cleanTarget(value);
      if (target && target !== current) {
        try {
          error = await api.checkImportTarget(editor.libraryId, value, packId);
        } catch (err) {
          error = String(err);
        }
      }
      const now = get().editors[packId]?.target;
      if (now?.value !== value) return;
      patch(packId, { target: { value, checked: value, error } });
    },

    moveTarget: async (packId, current) => {
      const editor = get().editors[packId];
      const typed = editor?.target;
      if (!editor || !typed || editor.saving) return false;
      const reason = busy(editor) ?? targetProblem(typed, current);
      if (reason) {
        patch(packId, { error: reason });
        return false;
      }
      const target = cleanTarget(typed.value);
      if (target === current) {
        patch(packId, { target: null });
        return true;
      }
      patch(packId, { saving: true, error: null });
      try {
        await api.setImportTarget(editor.libraryId, packId, target);
        patch(packId, { saving: false, target: null });
        await useImportStore.getState().fetchLibrary(editor.libraryId);
        return true;
      } catch (err) {
        patch(packId, { saving: false, error: String(err) });
        return false;
      }
    },
  };
});
