import { create } from 'zustand';
import { api } from '../api';
import type {
  ImportCandidate,
  ImportOffer,
  ImportPackSummary,
  ImportPreset,
  ImportProgress,
  ImportRequest,
  ImportRun,
  ImportScan,
  ImportSummary,
  ImportTree,
  ImportVariant,
  PlaceStep,
  SkippedEntry,
} from '../types';

// The Imports view (plan pack-import, UI): each import's original and packs, the active run's
// progress, errors, warnings and Cancel, and the Import pack dialog's draft.

/** What Cancel answers once a build holds the library's lock (`import::build::FINISHING`) */
export const FINISHING = "Finishing, can't cancel now";

/**
 * The step a build reports right after it takes the library's lock (`import::build`), past which
 * Cancel no longer works. The run is marked finishing from here to its end, whatever steps follow.
 */
export const LOCKED_STEP = 're-checking';

export function isLive(run: ImportRun): boolean {
  return run.status === 'queued' || run.status === 'running';
}

/** `runs` with `run` in place of the one with its id, newest first */
export function upsertRun(runs: ImportRun[], run: ImportRun): ImportRun[] {
  const rest = runs.filter((r) => r.id !== run.id);
  return [run, ...rest].sort((a, b) => b.createdAt - a.createdAt);
}

/**
 * `runs` with a listed `run` merged in. Runs are never deleted, so nothing held is dropped, and
 * a run held as ended stays so when the list still shows it live: the list was read before the
 * `import:updated` that ended it.
 */
export function mergeRun(runs: ImportRun[], run: ImportRun): ImportRun[] {
  const held = runs.find((r) => r.id === run.id);
  return held && !isLive(held) && isLive(run) ? runs : upsertRun(runs, run);
}

/** A file's name from its absolute path */
export function originalName(path: string): string {
  return path.split('/').filter(Boolean).pop() ?? path;
}

export interface PackRow {
  pack: ImportPackSummary;
  /** The pack's latest run */
  run: ImportRun | null;
  /** The latest `import:progress` of that run while it is running */
  progress: ImportProgress | null;
}

/** One original brought into one library, with every pack imported from it */
export interface ImportRow {
  key: string;
  libraryId: string;
  originalPath: string;
  name: string;
  packs: PackRow[];
}

/**
 * The Imports view's rows: one per original and library, newest first. Each `start_import`
 * records its own import, so the packs of every import of the same original are gathered in one
 * row, each with its latest run (`runs` newest first) and that run's live progress.
 */
export function importRows(
  imports: ImportSummary[],
  runs: ImportRun[],
  progress: Record<string, ImportProgress>
): ImportRow[] {
  const rows = new Map<string, ImportRow>();
  const sorted = [...imports].sort((a, b) => b.createdAt - a.createdAt);
  for (const imp of sorted) {
    const key = `${imp.libraryId}\n${imp.originalPath}`;
    let row = rows.get(key);
    if (!row) {
      row = {
        key,
        libraryId: imp.libraryId,
        originalPath: imp.originalPath,
        name: originalName(imp.originalPath),
        packs: [],
      };
      rows.set(key, row);
    }
    for (const pack of imp.packs) {
      const run =
        runs.find(
          (r) => r.libraryId === imp.libraryId && r.packs.some((p) => p.packId === pack.id)
        ) ?? null;
      const live = run && run.status === 'running' ? (progress[run.id] ?? null) : null;
      row.packs.push({ pack, run, progress: live });
    }
  }
  for (const row of rows.values())
    row.packs.sort((a, b) => a.pack.target.localeCompare(b.pack.target));
  return [...rows.values()];
}

/**
 * What a run's Cancel button shows: nothing once the run ended, Cancel while it is queued or
 * building in staging, and "Finishing, can't cancel now" once its build holds the lock: `finishing`
 * holds the runs that reported `LOCKED_STEP`, or whose Cancel was answered so.
 */
export function cancelState(
  run: ImportRun,
  finishing: ReadonlySet<string>
): 'none' | 'cancel' | 'finishing' {
  if (!isLive(run)) return 'none';
  return finishing.has(run.id) ? 'finishing' : 'cancel';
}

const STATUS_LABELS: Record<ImportRun['status'], string> = {
  queued: 'Queued',
  running: 'Running',
  done: 'Built',
  failed: 'Failed',
  cancelled: 'Cancelled',
  interrupted: 'Interrupted',
};

const PACK_LABELS: Record<ImportPackSummary['status'], string> = {
  built: 'Built',
  failed: 'Failed',
  never_built: 'Not built',
};

/** A pack's state: its live run's, else its own */
export function packStatus(row: PackRow): string {
  if (row.run && isLive(row.run)) return STATUS_LABELS[row.run.status];
  if (row.run?.status === 'cancelled' || row.run?.status === 'interrupted')
    return row.pack.status === 'built' ? PACK_LABELS.built : STATUS_LABELS[row.run.status];
  return PACK_LABELS[row.pack.status] ?? row.pack.status;
}

/** A live run's progress line: its step, with counts when it has them */
export function progressText(run: ImportRun, progress: ImportProgress | null): string {
  if (run.status === 'queued') return 'Waiting for the run before it';
  const step = progress?.step ?? run.phase ?? 'starting';
  const done = progress ? progress.done : run.progressDone;
  const total = progress ? progress.total : run.progressTotal;
  const counts = total > 0 ? ` ${done.toLocaleString()} / ${total.toLocaleString()}` : '';
  return `${step.charAt(0).toUpperCase()}${step.slice(1)}${counts}`;
}

/** 0-100, or null when the step has no counts */
export function progressPercent(progress: ImportProgress | null): number | null {
  if (!progress || progress.total <= 0) return null;
  return Math.min(100, (progress.done / progress.total) * 100);
}

const SKIP_REASONS: Record<SkippedEntry['reason'], string> = {
  absolute: 'an absolute path',
  escapes: 'climbs out of the pack',
  noName: 'names nothing',
  duplicate: 'a duplicate',
  hardlink: 'a hard link',
  symlink: 'a symbolic link',
  special: 'not a regular file',
};

/** A skipped entry, named by its path in the original (nested packages joined by ` > `) */
export function skippedText(entry: SkippedEntry): string {
  return `${entry.sourcePath.join(' > ')}: ${SKIP_REASONS[entry.reason] ?? entry.reason}`;
}

/**
 * Whether Escape closes the Imports view: not while the Import pack dialog is open over it, nor
 * when the dialog already took the key (`preventDefault`), whichever listener runs first.
 */
export function escapeClosesPanel(
  e: Pick<KeyboardEvent, 'key' | 'defaultPrevented'>,
  dialogOpen: boolean
): boolean {
  return e.key === 'Escape' && !e.defaultPrevented && !dialogOpen;
}

// ── The Import pack dialog's draft ────────────────────────────────────────

/** A candidate pack as the dialog edits it */
export interface DraftCandidate {
  name: string;
  preset: ImportPreset;
  ticked: boolean;
  target: string;
  /** Typed by you, so a publisher change leaves it alone */
  targetEdited: boolean;
  /** The target `targetError` was checked for; another value is waiting for its check */
  checkedTarget: string;
  /** Why the library refuses `checkedTarget` */
  targetError: string | null;
  variants: ImportVariant[];
  /** The ticked variant, or null (none ticked, or no variants) */
  variant: number | null;
  /** The extract step's sub-folder of a zip, for a Kenney pack */
  subpath: string | null;
  flatten: boolean;
  /** Whether the `hoist` step is on */
  hoistOn: boolean;
  /** The folder `hoist` moves up */
  hoist: string;
  /** The place steps the scan proposed, which its trees were made with */
  scannedPlace: PlaceStep[];
  /** The scan's target is `<Publisher>/<Pack>`, so it moves with the publisher */
  followsPublisher: boolean;
  /** Whether the preset converts models to GLB (`unity_glb`) */
  converts: boolean;
  /** An Unreal pack's `.uproject`, whose meshes its build exports (`export_unreal`), else null */
  unreal: string | null;
  title: string;
  version: string;
  license: string;
  lookupStorePage: boolean;
  /** The tree of the source and place steps in `treeKey` */
  tree: ImportTree | null;
  treeKey: string;
  /** Why `start_import` refused it */
  error: string | null;
  /** Its import was recorded and queued */
  imported: boolean;
}

export interface ImportDraft {
  libraryId: string;
  originalPath: string;
  kind: ImportScan['kind'];
  offers: ImportOffer[];
  /** The offer you import; a zip matching several rows is offered as each */
  offer: number;
  publisher: string;
  candidates: DraftCandidate[];
  /** The scan's steps that can't run here, by kind, with the reason */
  unavailable: Record<string, string>;
}

/** `<Publisher>/<Pack>`, or `<Pack>` while no publisher is known, as the scan names targets */
export function publisherTarget(publisher: string, name: string): string {
  const p = publisher.trim();
  return p ? `${p}/${name}` : name;
}

/** A candidate's place steps, in the order the build runs them */
export function placeSteps(c: Pick<DraftCandidate, 'flatten' | 'hoistOn' | 'hoist'>): PlaceStep[] {
  const steps: PlaceStep[] = [];
  if (c.flatten) steps.push({ kind: 'flatten' });
  if (c.hoistOn && c.hoist.trim()) steps.push({ kind: 'hoist', folder: c.hoist.trim() });
  return steps;
}

/** The folder the scan's `hoist` step moved up, or '' when it proposed none */
function scannedHoist(c: DraftCandidate): string {
  const hoist = c.scannedPlace.find((s) => s.kind === 'hoist');
  return hoist?.kind === 'hoist' ? hoist.folder : '';
}

/** Turns candidate `i`'s hoist on or off; on with no folder named, it takes the scan's */
export function setHoist(draft: ImportDraft, i: number, on: boolean): ImportDraft {
  const c = draft.candidates[i];
  const folder = on && !c.hoist.trim() ? scannedHoist(c) : c.hoist;
  return patchCandidate(draft, i, { hoistOn: on, hoist: folder });
}

/** The source of a candidate's tree: its ticked variant and sub-folder */
function source(c: DraftCandidate): { variant: string | null; subpath: string | null } {
  const v = c.variant === null ? null : c.variants[c.variant];
  if (v) return { variant: v.variant, subpath: v.subpath };
  return { variant: null, subpath: c.subpath };
}

/** What a candidate's tree depends on: its source and place steps */
export function treeKey(c: DraftCandidate): string {
  return JSON.stringify([source(c), placeSteps(c)]);
}

/** Whether a candidate's tree is for its current source and steps */
export function treeCurrent(c: DraftCandidate): boolean {
  return c.treeKey === treeKey(c);
}

function draftCandidate(c: ImportCandidate): DraftCandidate {
  const extract = c.steps.find((s) => s.kind.startsWith('extract'));
  const metadata = c.steps.find((s) => s.kind === 'metadata')?.options ?? {};
  const hoist = c.steps.find((s) => s.kind === 'hoist')?.options.folder;
  const flatten = c.steps.some((s) => s.kind === 'flatten');
  const scannedPlace: PlaceStep[] = [];
  if (flatten) scannedPlace.push({ kind: 'flatten' });
  if (typeof hoist === 'string') scannedPlace.push({ kind: 'hoist', folder: hoist });
  const variant = c.variants.findIndex((v) => v.ticked);
  const draft: DraftCandidate = {
    name: c.name,
    preset: c.preset,
    ticked: c.ticked,
    target: c.target,
    targetEdited: false,
    checkedTarget: c.target,
    targetError: c.targetError,
    variants: c.variants,
    variant: variant >= 0 ? variant : null,
    subpath: c.variants.length ? null : ((extract?.options.subpath as string | undefined) ?? null),
    flatten,
    hoistOn: typeof hoist === 'string',
    hoist: typeof hoist === 'string' ? hoist : '',
    scannedPlace,
    followsPublisher: c.targetFollowsPublisher,
    converts: c.steps.some((s) => s.kind === 'unity_glb'),
    unreal: c.steps.some((s) => s.kind === 'export_unreal') ? (c.project ?? '') : null,
    title: '',
    version: '',
    license: '',
    lookupStorePage: metadata.lookup_store_page === true,
    tree: c.tree,
    treeKey: '',
    error: null,
    imported: false,
  };
  draft.treeKey = c.tree ? treeKey(draft) : '';
  return draft;
}

/** The draft of offer `offer` of a scan */
export function draftFromScan(libraryId: string, scan: ImportScan, offer = 0): ImportDraft {
  const chosen = scan.offers[offer];
  return {
    libraryId,
    originalPath: scan.originalPath,
    kind: scan.kind,
    offers: scan.offers,
    offer,
    publisher: chosen?.candidates[0]?.publisher ?? '',
    candidates: chosen?.candidates.map(draftCandidate) ?? [],
    unavailable: scan.unavailable ?? {},
  };
}

/** The draft with candidate `i` changed by `patch` */
export function patchCandidate(
  draft: ImportDraft,
  i: number,
  patch: Partial<DraftCandidate>
): ImportDraft {
  const candidates = draft.candidates.map((c, j) => (j === i ? { ...c, ...patch } : c));
  return { ...draft, candidates };
}

/** Types a candidate's target, which a publisher change then leaves alone */
export function editTarget(draft: ImportDraft, i: number, target: string): ImportDraft {
  return patchCandidate(draft, i, { target, targetEdited: true, error: null });
}

/** Changes the publisher, moving each `<Publisher>/<Pack>` target you didn't type with it */
export function editPublisher(draft: ImportDraft, publisher: string): ImportDraft {
  const candidates = draft.candidates.map((c) =>
    c.followsPublisher && !c.targetEdited
      ? { ...c, target: publisherTarget(publisher, c.name), error: null }
      : c
  );
  return { ...draft, publisher, candidates };
}

/** Ticks variant `v` of candidate `i`, the one its build extracts (a Fab pack builds one) */
export function pickVariant(draft: ImportDraft, i: number, v: number): ImportDraft {
  const c = draft.candidates[i];
  const variants = c.variants.map((variant, j) => ({ ...variant, ticked: j === v }));
  const next = { ...c, variants, variant: v };
  // The scan's variant tree is for the place steps it proposed
  const scanned = JSON.stringify(placeSteps(next)) === JSON.stringify(c.scannedPlace);
  return patchCandidate(draft, i, {
    variants,
    variant: v,
    ...(scanned ? { tree: variants[v].tree, treeKey: treeKey(next) } : {}),
  });
}

/** Two targets that are the same or one holds the other, on whole path components */
export function nests(a: string, b: string): boolean {
  return a === b || a.startsWith(`${b}/`) || b.startsWith(`${a}/`);
}

/** A target as the library stores it: no empty or `.` components */
export function cleanTarget(target: string): string {
  return target
    .split('/')
    .filter((p) => p && p !== '.')
    .join('/');
}

/**
 * Why candidate `i` can't be imported as it is, or null. Refused targets name the other pack:
 * the library's reason (it exists, or nests with an imported pack), or another ticked
 * candidate's target it nests with.
 */
export function candidateProblem(draft: ImportDraft, i: number): string | null {
  const c = draft.candidates[i];
  if (!c.ticked || c.imported) return null;
  const target = cleanTarget(c.target);
  if (!target) return 'Name the folder to import into';
  if (c.checkedTarget !== c.target) return 'Checking the target…';
  if (c.targetError) return c.targetError;
  const other = draft.candidates.find(
    (o, j) => j !== i && o.ticked && nests(target, cleanTarget(o.target))
  );
  if (other) return `${target} would nest with ${cleanTarget(other.target)}, also ticked`;
  if (c.variants.length > 0 && c.variant === null) return 'Tick the variant to import';
  if (c.hoistOn && !c.hoist.trim()) return 'Name the folder to move up';
  if (!treeCurrent(c)) return 'Previewing the steps…';
  if (c.tree?.error) return `The build would fail: ${c.tree.error}`;
  return null;
}

/** Why Import is disabled, or null */
export function draftProblem(draft: ImportDraft): string | null {
  const ticked = draft.candidates.filter((c) => c.ticked && !c.imported);
  if (ticked.length === 0) return 'Tick a pack to import';
  if (!draft.publisher.trim()) return 'Name the publisher';
  const blocked = draft.candidates.findIndex((_, i) => candidateProblem(draft, i) !== null);
  if (blocked >= 0) return `${draft.candidates[blocked].name}: ${candidateProblem(draft, blocked)}`;
  return null;
}

/** The `start_import` request of candidate `c` */
export function importRequest(draft: ImportDraft, c: DraftCandidate): ImportRequest {
  const { variant, subpath } = source(c);
  const request: ImportRequest = {
    libraryId: draft.libraryId,
    originalPath: draft.originalPath,
    target: cleanTarget(c.target),
    publisher: draft.publisher.trim(),
    preset: c.preset,
    place: placeSteps(c),
  };
  if (variant) request.variant = variant;
  if (subpath) request.subpath = subpath;
  if (c.title.trim()) request.title = c.title.trim();
  if (c.version.trim()) request.version = c.version.trim();
  if (c.license.trim()) request.license = c.license.trim();
  if (c.preset === 'kenney') request.lookupStorePage = c.lookupStorePage;
  if (c.unreal !== null) request.unreal = true;
  return request;
}

// ── The store ─────────────────────────────────────────────────────────────

interface ImportStore {
  /** Each open library's imports */
  imports: Record<string, ImportSummary[]>;
  /** Every open library's runs, newest first */
  runs: ImportRun[];
  /** Each running run's latest `import:progress` */
  progress: Record<string, ImportProgress>;
  /** Runs whose Cancel answered "Finishing, can't cancel now" */
  finishing: Set<string>;
  /** Why the last Cancel or list failed */
  error: string | null;
  panelOpen: boolean;
  /** The open Import pack dialog: its original and library, while scanning or after */
  dialog: { originalPath: string; libraryId: string } | null;
  scanning: boolean;
  scanError: string | null;
  draft: ImportDraft | null;
  submitting: boolean;

  fetch: (libraryIds: string[]) => Promise<void>;
  fetchLibrary: (libraryId: string) => Promise<void>;
  /** Takes an `import:updated` run, refetching its library's imports once it ends */
  applyUpdate: (run: ImportRun) => void;
  applyProgress: (progress: ImportProgress) => void;
  cancel: (run: ImportRun) => Promise<void>;
  setPanelOpen: (open: boolean) => void;
  /** Opens the dialog over `originalPath` and scans it for `libraryId` */
  openDialog: (originalPath: string, libraryId: string) => Promise<void>;
  /** Closes the dialog, unless Import is submitting its packs */
  closeDialog: () => void;
  /** Scans the dialog's original again for another library */
  setLibrary: (libraryId: string) => Promise<void>;
  pickOffer: (offer: number) => void;
  /** Applies a pure draft change, such as `editTarget` or `editPublisher`, unless Import is submitting */
  edit: (change: (draft: ImportDraft) => ImportDraft) => void;
  /** Asks the library about candidate `i`'s target, unless it was asked already */
  checkTarget: (i: number) => Promise<void>;
  /** Previews candidate `i`'s tree for its source and steps, unless it is current */
  refreshTree: (i: number) => Promise<void>;
  /** Imports every ticked candidate, one `start_import` each; true when all were queued */
  submit: () => Promise<boolean>;
}

export const useImportStore = create<ImportStore>()((set, get) => {
  /**
   * Each library's latest `fetchLibrary` request. Fetches overlap (a run's queue and its end
   * both fetch), so an imports answer older than the latest request is dropped: one read before
   * a build committed would otherwise show its pack Not built after a newer read showed it Built.
   */
  const fetches = new Map<string, number>();

  /** The draft, when it is still the one of `libraryId` and `originalPath` */
  const sameDraft = (libraryId: string, originalPath: string) => {
    const draft = get().draft;
    return draft && draft.libraryId === libraryId && draft.originalPath === originalPath
      ? draft
      : null;
  };

  const scan = async (originalPath: string, libraryId: string) => {
    set({ dialog: { originalPath, libraryId }, scanning: true, scanError: null, draft: null });
    try {
      const result = await api.scanImport(libraryId, originalPath);
      const dialog = get().dialog;
      if (dialog?.originalPath !== originalPath || dialog.libraryId !== libraryId) return;
      set({ draft: draftFromScan(libraryId, result), scanning: false });
    } catch (err) {
      const dialog = get().dialog;
      if (dialog?.originalPath !== originalPath || dialog.libraryId !== libraryId) return;
      set({ scanError: String(err), scanning: false });
    }
  };

  return {
    imports: {},
    runs: [],
    progress: {},
    finishing: new Set(),
    error: null,
    panelOpen: false,
    dialog: null,
    scanning: false,
    scanError: null,
    draft: null,
    submitting: false,

    fetch: async (libraryIds) => {
      await Promise.all(libraryIds.map((id) => get().fetchLibrary(id)));
    },

    fetchLibrary: async (libraryId) => {
      const request = (fetches.get(libraryId) ?? 0) + 1;
      fetches.set(libraryId, request);
      try {
        const [imports, runs] = await Promise.all([
          api.listImports(libraryId),
          api.listImportRuns(libraryId),
        ]);
        const latest = fetches.get(libraryId) === request;
        // Runs merge safely whatever their age (`mergeRun`); imports are replaced, so only the
        // latest request's answer is kept
        set((s) => ({
          ...(latest ? { imports: { ...s.imports, [libraryId]: imports } } : {}),
          runs: runs.reduce(mergeRun, s.runs),
        }));
      } catch (err) {
        set({ error: String(err) });
      }
    },

    applyUpdate: (run) => {
      set((s) => {
        const next: Partial<ImportStore> = { runs: upsertRun(s.runs, run) };
        if (!isLive(run)) {
          const { [run.id]: _, ...progress } = s.progress;
          const finishing = new Set(s.finishing);
          finishing.delete(run.id);
          Object.assign(next, { progress, finishing });
        }
        return next;
      });
      // A run's start records its import, and its end changes its packs' status
      if (run.status === 'queued' || !isLive(run)) void get().fetchLibrary(run.libraryId);
    },

    applyProgress: (progress) =>
      set((s) => ({
        progress: { ...s.progress, [progress.runId]: progress },
        ...(progress.step === LOCKED_STEP
          ? { finishing: new Set(s.finishing).add(progress.runId) }
          : {}),
      })),

    cancel: async (run) => {
      set({ error: null });
      try {
        await api.cancelImportRun(run.libraryId, run.id);
      } catch (err) {
        const message = String(err);
        if (message.includes(FINISHING))
          set((s) => ({ finishing: new Set(s.finishing).add(run.id) }));
        else set({ error: message });
      }
    },

    setPanelOpen: (panelOpen) => set({ panelOpen }),

    openDialog: (originalPath, libraryId) => scan(originalPath, libraryId),

    // Not while Import runs: each pack's refusal is shown on its candidate, which must stay open
    closeDialog: () => {
      if (get().submitting) return;
      set({ dialog: null, draft: null, scanning: false, scanError: null });
    },

    setLibrary: async (libraryId) => {
      const dialog = get().dialog;
      if (dialog && dialog.libraryId !== libraryId) await scan(dialog.originalPath, libraryId);
    },

    pickOffer: (offer) => {
      const draft = get().draft;
      if (!draft || draft.offer === offer) return;
      const scanned: ImportScan = {
        originalPath: draft.originalPath,
        kind: draft.kind,
        offers: draft.offers,
        unavailable: draft.unavailable,
      };
      set({ draft: draftFromScan(draft.libraryId, scanned, offer) });
    },

    // Not while Import runs: it sends each pack as it was when Import was clicked
    edit: (change) => {
      const draft = get().draft;
      if (draft && !get().submitting) set({ draft: change(draft) });
    },

    checkTarget: async (i) => {
      const draft = get().draft;
      const c = draft?.candidates[i];
      if (!draft || !c || c.checkedTarget === c.target) return;
      const target = c.target;
      let targetError: string | null;
      if (!cleanTarget(target)) targetError = null;
      else {
        try {
          targetError = await api.checkImportTarget(draft.libraryId, target);
        } catch (err) {
          targetError = String(err);
        }
      }
      const now = sameDraft(draft.libraryId, draft.originalPath);
      if (!now || now.candidates[i]?.target !== target) return;
      set({ draft: patchCandidate(now, i, { checkedTarget: target, targetError }) });
    },

    refreshTree: async (i) => {
      const draft = get().draft;
      const c = draft?.candidates[i];
      if (!draft || !c || treeCurrent(c)) return;
      if (c.variants.length > 0 && c.variant === null) return;
      const key = treeKey(c);
      const { variant, subpath } = source(c);
      let tree: ImportTree;
      try {
        tree = await api.previewImport(
          draft.libraryId,
          draft.originalPath,
          variant,
          subpath,
          placeSteps(c),
          c.unreal !== null
        );
      } catch (err) {
        tree = { folders: [], files: 0, glbs: 0, skipped: [], error: String(err) };
      }
      const now = sameDraft(draft.libraryId, draft.originalPath);
      if (!now || !now.candidates[i] || treeKey(now.candidates[i]) !== key) return;
      set({ draft: patchCandidate(now, i, { tree, treeKey: key }) });
    },

    submit: async () => {
      const draft = get().draft;
      if (!draft || draftProblem(draft) !== null || get().submitting) return false;
      set({ submitting: true });
      let ok = true;
      // One at a time, so each insert sees the packs recorded before it
      for (let i = 0; i < draft.candidates.length; i++) {
        const c = draft.candidates[i];
        if (!c.ticked || c.imported) continue;
        let patch: Partial<DraftCandidate>;
        try {
          const run = await api.startImport(importRequest(draft, c));
          set((s) => ({ runs: upsertRun(s.runs, run) }));
          patch = { imported: true, error: null };
        } catch (err) {
          ok = false;
          patch = { error: String(err) };
        }
        const now = sameDraft(draft.libraryId, draft.originalPath);
        if (now) set({ draft: patchCandidate(now, i, patch) });
      }
      set({ submitting: false });
      await get().fetchLibrary(draft.libraryId);
      if (ok) get().closeDialog();
      return ok;
    },
  };
});
