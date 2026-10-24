import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import type {
  DriftChosen,
  DriftDifference,
  ImportPackSteps,
  ImportPackSummary,
  ImportRun,
  ImportStepRecord,
  ImportSummary,
} from '../types';
import { useImportStore } from './imports';
import {
  BUSY,
  DRIFT_STALE,
  STALE,
  busyReason,
  chooseDrift,
  choiceLabel,
  chosenFrom,
  differenceText,
  driftDraft,
  driftOpen,
  draftFromSteps,
  draftProblem,
  fieldValue,
  isDirty,
  latestRun,
  normalizeStep,
  optionFields,
  preparationPack,
  setOption,
  stepLabel,
  stepProblem,
  stepsToSave,
  targetLocked,
  targetProblem,
  toggleProblem,
  toggleStep,
  usePreparationStore,
} from './preparation';

const real = { ...api };
const initialImports = useImportStore.getState();
const initial = usePreparationStore.getState();

afterEach(() => {
  Object.assign(api, real);
  useImportStore.setState(initialImports, true);
  usePreparationStore.setState(initial, true);
});

const step = (
  kind: string,
  options: Record<string, unknown> = {},
  enabled = true
): ImportStepRecord => ({ kind, enabled, options });

/** The golden Unity pack's steps, as the library gives them (options' keys sorted) */
const golden = (): ImportStepRecord[] => [
  step('extract_unitypackage'),
  step('flatten'),
  step('hoist', { folder: 'Art' }),
  step('unity_glb', { max_texture: 1024 }),
  step('metadata', { preset: 'unity', title: 'Pack' }),
];

const read = (
  steps = golden(),
  recipeRev = 0,
  unavailable = {},
  kept: string[] = []
): ImportPackSteps => ({
  recipeRev,
  steps,
  unavailable,
  kept,
});

function run(id: string, packId: string, patch: Partial<ImportRun> = {}): ImportRun {
  return {
    id,
    libraryId: 'lib',
    kind: 'rebuild',
    packs: [{ packId, recipeRev: 0 }],
    status: 'queued',
    phase: null,
    progressDone: 0,
    progressTotal: 0,
    error: null,
    warnings: [],
    createdAt: 0,
    startedAt: null,
    finishedAt: null,
    ...patch,
  };
}

const pack = (patch: Partial<ImportPackSummary> = {}): ImportPackSummary => ({
  id: 'p1',
  target: 'Pub/Pack',
  preset: 'unity',
  status: 'built',
  builtAt: 1,
  ...patch,
});

const hoistAt = (steps: ImportStepRecord[]) => steps.findIndex((s) => s.kind === 'hoist');

describe('finding a pack folder’s import', () => {
  test('a folder that is a pack’s target has its import and pack, any other has none', () => {
    const imports: Record<string, ImportSummary[]> = {
      lib: [
        {
          id: 'i1',
          libraryId: 'lib',
          originalPath: '/dl/pack.unitypackage',
          kind: 'unitypackage',
          createdAt: 0,
          packs: [pack()],
        },
      ],
    };
    expect(preparationPack(imports, 'lib', 'Pub/Pack')?.original.id).toBe('i1');
    expect(preparationPack(imports, 'lib', 'Pub/Pack')?.pack.id).toBe('p1');
    expect(preparationPack(imports, 'lib', 'Pub')).toBeNull();
    expect(preparationPack(imports, 'lib', 'Pub/Pack/Art')).toBeNull();
    expect(preparationPack(imports, 'other', 'Pub/Pack')).toBeNull();
  });

  test('the latest run is the newest one naming the pack in its library', () => {
    const runs = [run('new', 'p1'), run('other', 'p2'), run('old', 'p1')];
    expect(latestRun(runs, 'lib', 'p1')?.id).toBe('new');
    expect(latestRun(runs, 'elsewhere', 'p1')).toBeNull();
  });
});

describe('busy packs', () => {
  test('a queued or running run for the pack refuses edits with the engine’s reason', () => {
    expect(busyReason([run('r', 'p1', { status: 'queued' })], 'lib', 'p1')).toBe(BUSY);
    expect(busyReason([run('r', 'p1', { status: 'running' })], 'lib', 'p1')).toBe(BUSY);
    expect(BUSY).toBe('An import run is building this pack');
  });

  test('an ended run, another pack’s run or another library’s run doesn’t', () => {
    for (const status of ['done', 'failed', 'cancelled', 'interrupted'] as const)
      expect(busyReason([run('r', 'p1', { status })], 'lib', 'p1')).toBeNull();
    expect(busyReason([run('r', 'p2')], 'lib', 'p1')).toBeNull();
    expect(busyReason([run('r', 'p1', { libraryId: 'b' })], 'lib', 'p1')).toBeNull();
  });
});

describe('the steps', () => {
  test('each step says what it does, naming its variant, folder or source path', () => {
    expect(stepLabel(step('extract_unitypackage'))).toBe('Extract the package');
    expect(stepLabel(step('extract_unitypackage', { variant: 'URP.unitypackage' }))).toBe(
      'Extract URP.unitypackage'
    );
    expect(stepLabel(step('extract_zip', { subpath: 'Models' }))).toBe(
      'Extract Models from the zip'
    );
    expect(stepLabel(step('rename', { source: ['Art/Rock.fbx'], name: 'Boulder.fbx' }))).toBe(
      'Rename Art/Rock.fbx'
    );
    expect(stepLabel(step('exclude', { source: ['Inner.unitypackage', 'Art/A.png'] }))).toBe(
      'Leave out Inner.unitypackage > Art/A.png'
    );
    expect(stepLabel(step('metadata', { preset: 'kenney' }))).toBe(
      'Write datapackage.json (kenney)'
    );
    expect(stepLabel(step('unreal'))).toBe('unreal');
  });

  test('each kind offers its options; a whole license entry and others are kept as they are', () => {
    const keys = (s: ImportStepRecord) => optionFields(s).map((f) => f.key);
    expect(keys(step('hoist', { folder: 'Art' }))).toEqual(['folder']);
    expect(keys(step('rename', { source: ['a'], name: 'b' }))).toEqual(['name']);
    expect(keys(step('exclude', { source: ['a'] }))).toEqual([]);
    expect(keys(step('flatten'))).toEqual([]);
    expect(keys(step('unity_glb'))).toEqual(['max_texture', 'force']);
    expect(keys(step('metadata', { preset: 'unity', license: 'CC0-1.0' }))).toEqual([
      'title',
      'version',
      'publisher',
      'authors',
      'license',
    ]);
    expect(
      keys(step('metadata', { preset: 'fab', license: { name: 'Fab', path: 'L.txt' } }))
    ).not.toContain('license');
    expect(keys(step('metadata', { preset: 'kenney' }))).toContain('lookup_store_page');
  });

  test('fields show their option as text, a list joined, a flag as a box', () => {
    const meta = step('metadata', {
      preset: 'kenney',
      authors: ['A', 'B'],
      lookup_store_page: true,
    });
    const [title, , , authors] = optionFields(meta);
    expect(fieldValue(meta, title)).toBe('');
    expect(fieldValue(meta, authors)).toBe('A, B');
    expect(fieldValue(meta, optionFields(meta).at(-1)!)).toBe(true);
    const glb = step('unity_glb', { max_texture: 1024 });
    expect(fieldValue(glb, optionFields(glb)[0])).toBe('1024');
  });

  test('saving parses typed numbers and lists and leaves empty metadata fields out', () => {
    const meta = normalizeStep(
      step('metadata', {
        preset: 'unity',
        title: '  ',
        version: ' 1.2 ',
        authors: 'Ann,  Bo , ',
        sources: [{ path: 'https://x' }],
      })
    );
    expect(meta.options).toEqual({
      preset: 'unity',
      version: '1.2',
      authors: ['Ann', 'Bo'],
      sources: [{ path: 'https://x' }],
    });
    expect(normalizeStep(step('unity_glb', { max_texture: '2048' })).options.max_texture).toBe(
      2048
    );
    // A hoist folder isn't optional: an empty one stays, for stepProblem to refuse
    expect(normalizeStep(step('hoist', { folder: '' })).options).toEqual({ folder: '' });
  });

  test('options a build couldn’t run are refused before they are sent', () => {
    expect(stepProblem(step('hoist', { folder: ' ' }))).toBe('Name the folder to move up');
    expect(stepProblem(step('hoist', { folder: 'Art' }))).toBeNull();
    expect(stepProblem(step('rename', { source: ['a'], name: '' }))).toBe('Name the file');
    expect(stepProblem(step('rename', { source: ['a'], name: 'Sub/b.png' }))).toContain(
      'without a folder'
    );
    for (const max of ['8', '20000', '1.5', 'big'])
      expect(stepProblem(step('unity_glb', { max_texture: max }))).toContain('16 to 16384');
    expect(stepProblem(step('unity_glb', { max_texture: '512' }))).toBeNull();
    expect(stepProblem(step('unity_glb'))).toBeNull();

    const draft = setOption(draftFromSteps(read()), hoistAt(golden()), 'folder', '');
    expect(draftProblem(draft)).toBe(
      'Move a folder up to the pack root: Name the folder to move up'
    );
  });
});

describe('editing the draft', () => {
  test('turning hoist off is an edit, and turning it back on undoes it', () => {
    const draft = draftFromSteps(read());
    expect(isDirty(draft)).toBe(false);
    const off = toggleStep(draft, hoistAt(golden()), false);
    expect(off.steps[hoistAt(golden())].enabled).toBe(false);
    expect(isDirty(off)).toBe(true);
    expect(stepsToSave(off).map((s) => [s.kind, s.enabled])).toEqual([
      ['extract_unitypackage', true],
      ['flatten', true],
      ['hoist', false],
      ['unity_glb', true],
      ['metadata', true],
    ]);
    expect(isDirty(toggleStep(off, hoistAt(golden()), true))).toBe(false);
  });

  test('an option typed back to its saved value is no edit, whatever the key order', () => {
    const draft = draftFromSteps(read([step('metadata', { preset: 'unity', title: 'Pack' })]));
    const typed = setOption(setOption(draft, 0, 'version', '2'), 0, 'title', 'Pack');
    expect(isDirty(typed)).toBe(true);
    expect(isDirty(setOption(typed, 0, 'version', ''))).toBe(false);
    // The library gives options back with their keys sorted
    const saved = { ...draft, saved: [step('metadata', { title: 'Pack', preset: 'unity' })] };
    expect(isDirty(saved)).toBe(false);
  });

  test('the extract step stays on, and a step whose tool is missing can only go off', () => {
    const unavailable = { unity_glb: 'Blender 5.2 is not installed' };
    const steps = [...golden(), step('unity_glb', {}, false)];
    const draft = draftFromSteps(read(steps, 0, unavailable));
    expect(toggleProblem(steps[0], unavailable)).toBe('A pack is built from its extract step');
    expect(toggleStep(draft, 0, false)).toBe(draft);
    expect(toggleProblem(steps[3], unavailable)).toBeNull();
    expect(toggleStep(draft, 3, false).steps[3].enabled).toBe(false);
    expect(toggleProblem(steps[5], unavailable)).toBe('Blender 5.2 is not installed');
    expect(toggleStep(draft, 5, true)).toBe(draft);
  });
});

describe('the target', () => {
  test('a built pack’s folder doesn’t move; a pack never built, or whose first build failed, can', () => {
    expect(targetLocked(pack())).toContain("can't move yet");
    expect(targetLocked(pack({ status: 'never_built', builtAt: null }))).toBeNull();
    expect(targetLocked(pack({ status: 'failed', builtAt: null }))).toBeNull();
    // Marked failed by a swap recovery, it still has its committed build
    expect(targetLocked(pack({ status: 'failed', builtAt: 1 }))).toContain("can't move yet");
  });

  test('a typed target waits for its check and shows the library’s reason', () => {
    const typed = (value: string, checked = value, error: string | null = null) => ({
      value,
      checked,
      error,
    });
    expect(targetProblem(typed('/./'), 'Pub/Pack')).toBe('Name the folder to import into');
    expect(targetProblem(typed('Pub/Pack/', ''), 'Pub/Pack')).toBeNull();
    expect(targetProblem(typed('Pub/Other', 'Pub/Oth'), 'Pub/Pack')).toBe('Checking the target…');
    const nests = 'Pub/Other would nest with the pack at Pub/Other/Sub';
    expect(targetProblem(typed('Pub/Other', 'Pub/Other', nests), 'Pub/Pack')).toBe(nests);
    expect(targetProblem(typed('Pub/Other'), 'Pub/Pack')).toBeNull();
  });
});

describe('the editor store', () => {
  const editor = () => usePreparationStore.getState().editors.p1;
  const store = () => usePreparationStore.getState();

  test('opening reads the steps once per pack, for both places that show them', async () => {
    let reads = 0;
    api.getImportSteps = async (libraryId, packId) => {
      reads++;
      expect([libraryId, packId]).toEqual(['lib', 'p1']);
      return read();
    };
    await store().open('lib', 'p1');
    expect(editor().draft?.steps).toEqual(golden());
    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));

    // The Imports view opens the editor the pack's details hold: the edit is still there
    await store().open('lib', 'p1');
    expect(reads).toBe(2);
    expect(isDirty(editor().draft!)).toBe(true);
    expect(Object.keys(store().editors)).toEqual(['p1']);
  });

  test('saving sends the parsed steps with the recipe_rev they were read at', async () => {
    api.getImportSteps = async () => read(golden(), 3);
    const sent: unknown[] = [];
    api.setImportSteps = async (libraryId, packId, steps, recipeRev) => {
      sent.push({ libraryId, packId, steps, recipeRev });
      return recipeRev + 1;
    };
    await store().open('lib', 'p1');
    expect(await store().save('p1')).toBe(true);
    expect(sent).toEqual([]); // nothing to save

    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));
    store().edit('p1', (d) => setOption(d, 3, 'max_texture', '512'));
    expect(await store().save('p1')).toBe(true);
    const expected = golden();
    expected[2].enabled = false;
    expected[3].options = { max_texture: 512 };
    expect(sent).toEqual([{ libraryId: 'lib', packId: 'p1', steps: expected, recipeRev: 3 }]);
    expect(editor().draft?.recipeRev).toBe(4);
    expect(isDirty(editor().draft!)).toBe(false);
  });

  test('while a run for the pack is live, nothing is edited, saved or rebuilt, and the reason shows', async () => {
    api.getImportSteps = async () => read();
    let calls = 0;
    api.setImportSteps = async () => ++calls;
    api.rebuildImportPack = async () => {
      calls++;
      return run('r', 'p1');
    };
    await store().open('lib', 'p1');
    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));
    useImportStore.setState({ runs: [run('live', 'p1', { status: 'running' })] });

    store().edit('p1', (d) => toggleStep(d, 1, false));
    expect(editor().draft?.steps[1].enabled).toBe(true);
    expect(await store().save('p1')).toBe(false);
    expect(editor().error).toBe(BUSY);
    expect(await store().rebuild('p1')).toBe(false);
    expect(editor().error).toBe(BUSY);
    expect(calls).toBe(0);
  });

  test('a refusal from the library shows its reason and keeps the edits', async () => {
    api.getImportSteps = async () => read();
    api.setImportSteps = async () => {
      throw BUSY;
    };
    await store().open('lib', 'p1');
    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));
    expect(await store().save('p1')).toBe(false);
    expect(editor().error).toBe(BUSY);
    expect(isDirty(editor().draft!)).toBe(true);
  });

  test('steps that moved on since they were read are read again, not overwritten', async () => {
    // An in-app rename appended a step after the editor read the steps
    const renamed = [...golden(), step('rename', { source: ['Art/Rock.fbx'], name: 'Big.fbx' })];
    let current = read(golden(), 0);
    api.getImportSteps = async () => current;
    api.setImportSteps = async () => {
      throw `${STALE}: read them again`;
    };
    await store().open('lib', 'p1');
    current = read(renamed, 1);
    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));
    expect(await store().save('p1')).toBe(false);
    expect(editor().draft?.steps).toEqual(renamed);
    expect(editor().draft?.recipeRev).toBe(1);
    expect(editor().error).toContain('shown again');
  });

  test('Rebuild saves the edits first, then queues the run, which makes the pack busy', async () => {
    api.getImportSteps = async () => read();
    const order: string[] = [];
    api.setImportSteps = async (_l, _p, steps) => {
      order.push(`save hoist=${steps[2].enabled}`);
      return 1;
    };
    api.checkImportDrift = async () => {
      order.push('check');
      return [];
    };
    api.rebuildImportPack = async (libraryId, packId, drift) => {
      order.push(`rebuild ${libraryId}/${packId} ${JSON.stringify(drift)}`);
      return run('r1', packId, { packs: [{ packId, recipeRev: 1 }] });
    };
    await store().open('lib', 'p1');
    store().edit('p1', (d) => toggleStep(d, hoistAt(d.steps), false));
    expect(await store().rebuild('p1')).toBe(true);
    expect(order).toEqual(['save hoist=false', 'check', 'rebuild lib/p1 []']);
    expect(editor().drift).toBeNull();
    expect(busyReason(useImportStore.getState().runs, 'lib', 'p1')).toBe(BUSY);
  });

  test('Rebuild with invalid edits sends nothing', async () => {
    api.getImportSteps = async () => read();
    let calls = 0;
    api.setImportSteps = async () => ++calls;
    api.rebuildImportPack = async () => {
      calls++;
      return run('r', 'p1');
    };
    await store().open('lib', 'p1');
    store().edit('p1', (d) => setOption(d, hoistAt(d.steps), 'folder', ''));
    expect(await store().rebuild('p1')).toBe(false);
    expect(editor().error).toContain('Name the folder to move up');
    expect(calls).toBe(0);
  });

  test('library:updated reads unedited editors again and leaves edited ones alone', async () => {
    let current = read(golden(), 0);
    api.getImportSteps = async () => current;
    await store().open('lib', 'p1');
    await store().open('lib', 'p2');
    store().edit('p2', (d) => toggleStep(d, hoistAt(d.steps), false));
    const renamed = [...golden(), step('exclude', { source: ['Art/A.png'] })];
    current = read(renamed, 1);
    await store().refetch();
    expect(editor().draft?.steps).toEqual(renamed);
    expect(store().editors.p2.draft?.steps[hoistAt(golden())].enabled).toBe(false);
  });

  test('an editor nobody shows is dropped, unless it holds edits, and never read again', async () => {
    const reads: string[] = [];
    api.getImportSteps = async (_libraryId, packId) => {
      reads.push(packId);
      return read();
    };
    // The pack's details and its Imports view row both show p1
    await store().open('lib', 'p1');
    await store().open('lib', 'p1');
    await store().open('lib', 'p2');
    expect(editor().mounts).toBe(2);

    store().close('p1');
    expect(editor().mounts).toBe(1);
    store().close('p1');
    expect(store().editors.p1).toBeUndefined();

    // Closed with edits, p2 keeps them for its next open but isn't read on library:updated
    store().edit('p2', (d) => toggleStep(d, hoistAt(d.steps), false));
    store().close('p2');
    expect(store().editors.p2.mounts).toBe(0);
    reads.length = 0;
    await store().refetch();
    expect(reads).toEqual([]);
    await store().open('lib', 'p2');
    expect(store().editors.p2.mounts).toBe(1);
    expect(isDirty(store().editors.p2.draft!)).toBe(true);

    // Once its edits are reverted, closing it drops it
    store().revert('p2');
    store().close('p2');
    expect(store().editors).toEqual({});
  });

  test('a typed target is checked as the pack’s own and moved, then imports are read again', async () => {
    api.getImportSteps = async () => read();
    const checks: unknown[] = [];
    api.checkImportTarget = async (libraryId, target, packId) => {
      checks.push([libraryId, target, packId]);
      return target === 'Pub' ? 'Pub already exists in the library' : null;
    };
    const moves: unknown[] = [];
    api.setImportTarget = async (libraryId, packId, target) => {
      moves.push([libraryId, packId, target]);
    };
    let listed = 0;
    api.listImports = async () => {
      listed++;
      return [];
    };
    api.listImportRuns = async () => [];
    await store().open('lib', 'p1');

    store().editTarget('p1', 'Pub');
    expect(await store().moveTarget('p1', 'Pub/Pack')).toBe(false);
    expect(editor().error).toBe('Checking the target…');
    await store().checkTarget('p1', 'Pub/Pack');
    expect(targetProblem(editor().target!, 'Pub/Pack')).toBe('Pub already exists in the library');

    store().editTarget('p1', 'Pub/Other/');
    await store().checkTarget('p1', 'Pub/Pack');
    expect(await store().moveTarget('p1', 'Pub/Pack')).toBe(true);
    expect(checks).toEqual([
      ['lib', 'Pub', 'p1'],
      ['lib', 'Pub/Other/', 'p1'],
    ]);
    expect(moves).toEqual([['lib', 'p1', 'Pub/Other']]);
    expect(editor().target).toBeNull();
    expect(listed).toBe(1);
  });
});

/** An outside change as `check_import_drift` gives it */
const difference = (
  path: string,
  change: DriftDifference['change'],
  kind: DriftDifference['kind'],
  choices: DriftDifference['choices'],
  reason: string | null = null
): DriftDifference => ({
  path: `Pub/Pack/${path}`,
  change,
  kind,
  size: change === 'missing' ? null : 4,
  sha256: change === 'missing' ? null : `sha-${path}`,
  choices,
  reason,
});

const outside = (): DriftDifference[] => [
  difference('Notes.txt', 'added', 'outside', ['keep', 'discard']),
  difference('Readme.txt', 'changed', 'extracted', ['keep', 'recipe']),
  difference(
    'Models/SM_Log.glb',
    'missing',
    'generated',
    ['restore'],
    'No step drops one GLB: delete its model in the app, or turn the convert step off'
  ),
];

describe('outside changes', () => {
  const editor = () => usePreparationStore.getState().editors.p1;
  const store = () => usePreparationStore.getState();

  test('each difference starts on its default, and takes only a choice it offers', () => {
    const draft = driftDraft(outside());
    expect(draft.choices).toEqual({
      'Pub/Pack/Notes.txt': 'keep',
      'Pub/Pack/Readme.txt': 'keep',
      'Pub/Pack/Models/SM_Log.glb': 'restore',
    });
    const recipe = chooseDrift(draft, 'Pub/Pack/Readme.txt', 'recipe');
    expect(recipe.choices['Pub/Pack/Readme.txt']).toBe('recipe');
    // A generated GLB can't be kept, nor a file be answered that isn't listed
    expect(chooseDrift(recipe, 'Pub/Pack/Models/SM_Log.glb', 'keep')).toBe(recipe);
    expect(chooseDrift(recipe, 'Pub/Pack/Other.txt', 'keep')).toBe(recipe);
    expect(chosenFrom(recipe)).toEqual<DriftChosen[]>([
      { path: 'Pub/Pack/Notes.txt', change: 'added', sha256: 'sha-Notes.txt', choice: 'keep' },
      {
        path: 'Pub/Pack/Readme.txt',
        change: 'changed',
        sha256: 'sha-Readme.txt',
        choice: 'recipe',
      },
      { path: 'Pub/Pack/Models/SM_Log.glb', change: 'missing', sha256: null, choice: 'restore' },
    ]);
  });

  test('the dialog names each change and choice for the file it is about', () => {
    const [added, changed, missing] = outside();
    expect(differenceText(added)).toBe('Added, not from the original');
    expect(differenceText(changed)).toBe('Changed, from the original');
    expect(differenceText(missing)).toBe('Missing, a converted GLB');
    expect(choiceLabel('keep', added)).toBe('Keep');
    expect(choiceLabel('keep', changed)).toBe('Keep yours');
    expect(choiceLabel('recipe', changed)).toBe("Use the recipe's");
    expect(choiceLabel('exclude', missing)).toBe('Leave it out');
    expect(choiceLabel('metadataOff', missing)).toBe('Turn the metadata step off');
  });

  test('Rebuild with outside changes opens the dialog, and its choices go with the run', async () => {
    api.getImportSteps = async () => read();
    api.checkImportDrift = async () => outside();
    const sent: DriftChosen[][] = [];
    api.rebuildImportPack = async (_l, packId, drift = []) => {
      sent.push(drift);
      return run('r1', packId);
    };
    await store().open('lib', 'p1');
    expect(await store().rebuild('p1')).toBe(false);
    expect(sent).toEqual([]);
    expect(driftOpen()).toBe(true);
    expect(editor().drift?.differences).toEqual(outside());

    store().chooseDrift('p1', 'Pub/Pack/Notes.txt', 'discard');
    expect(await store().confirmDrift('p1')).toBe(true);
    expect(sent).toEqual([
      chosenFrom(chooseDrift(driftDraft(outside()), 'Pub/Pack/Notes.txt', 'discard')),
    ]);
    expect(sent[0][0].choice).toBe('discard');
    expect(editor().drift).toBeNull();
    expect(busyReason(useImportStore.getState().runs, 'lib', 'p1')).toBe(BUSY);
  });

  test('Cancel closes the dialog and queues nothing', async () => {
    api.getImportSteps = async () => read();
    api.checkImportDrift = async () => outside();
    let queued = 0;
    api.rebuildImportPack = async () => {
      queued++;
      return run('r1', 'p1');
    };
    await store().open('lib', 'p1');
    await store().rebuild('p1');
    store().closeDrift('p1');
    expect(editor().drift).toBeNull();
    expect(driftOpen()).toBe(false);
    expect(queued).toBe(0);
  });

  test('a folder that changed again since its check shows the dialog again with what it holds now', async () => {
    api.getImportSteps = async () => read();
    let found = outside().slice(0, 1);
    api.checkImportDrift = async () => found;
    api.rebuildImportPack = async () => {
      throw `${DRIFT_STALE}: check it again`;
    };
    await store().open('lib', 'p1');
    await store().rebuild('p1');
    found = outside();
    expect(await store().confirmDrift('p1')).toBe(false);
    expect(editor().drift?.differences).toEqual(outside());
    expect(editor().drift?.notice).toContain('changed again');
    expect(editor().saving).toBe(false);
  });

  test('another refusal keeps the dialog and shows why', async () => {
    api.getImportSteps = async () => read();
    api.checkImportDrift = async () => outside();
    api.rebuildImportPack = async () => {
      throw BUSY;
    };
    await store().open('lib', 'p1');
    await store().rebuild('p1');
    expect(await store().confirmDrift('p1')).toBe(false);
    expect(editor().drift?.notice).toBe(BUSY);
  });

  test("the kept files are read with the steps, for the panel's list", async () => {
    api.getImportSteps = async () => read(golden(), 0, {}, ['Pub/Pack/Notes.txt']);
    await store().open('lib', 'p1');
    expect(editor().draft?.kept).toEqual(['Pub/Pack/Notes.txt']);
  });
});
