import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import type {
  ImportCandidate,
  ImportProgress,
  ImportRequest,
  ImportRun,
  ImportScan,
  ImportSummary,
  ImportTree,
} from '../types';
import {
  FINISHING,
  LOCKED_STEP,
  cancelState,
  candidateProblem,
  draftFromScan,
  draftProblem,
  editPublisher,
  editTarget,
  escapeClosesPanel,
  importRequest,
  importRows,
  mergeRun,
  packStatus,
  patchCandidate,
  pickVariant,
  placeSteps,
  progressText,
  setHoist,
  skippedText,
  treeCurrent,
  useImportStore,
} from './imports';

const real = { ...api };
const initial = useImportStore.getState();

afterEach(() => {
  Object.assign(api, real);
  useImportStore.setState(initial, true);
});

const tree = (files: number, error: string | null = null): ImportTree => ({
  folders: [{ path: '', files, total: files }],
  files,
  glbs: 0,
  skipped: [],
  error,
});

function run(id: string, packIds: string[], patch: Partial<ImportRun> = {}): ImportRun {
  return {
    id,
    libraryId: 'lib',
    kind: 'import',
    packs: packIds.map((packId) => ({ packId, recipeRev: 0 })),
    status: 'running',
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

function progress(runId: string, step: string, done = 0, total = 0): ImportProgress {
  return { runId, libraryId: 'lib', packId: 'p', step, done, total };
}

const fabSteps = (variant: string | null) => [
  { kind: 'extract_unitypackage', enabled: true, options: variant ? { variant } : {} },
  { kind: 'flatten', enabled: true, options: {} },
  { kind: 'hoist', enabled: true, options: { folder: 'Art' } },
  { kind: 'unity_glb', enabled: true, options: { max_texture: 1024 } },
  { kind: 'metadata', enabled: true, options: { preset: 'fab', publisher: 'Studio' } },
];

const fab: ImportCandidate = {
  name: 'Forest',
  ticked: true,
  preset: 'fab',
  publisher: 'Studio',
  target: 'Studio/Forest',
  targetError: null,
  targetFollowsPublisher: true,
  variants: [
    {
      variant: 'Forest_BiRP.unitypackage',
      subpath: null,
      pipeline: 'built-in',
      ticked: false,
      tree: tree(3),
    },
    {
      variant: 'Forest_URP.unitypackage',
      subpath: null,
      pipeline: 'urp',
      ticked: true,
      tree: tree(4),
    },
  ],
  steps: fabSteps('Forest_URP.unitypackage'),
  tree: tree(4),
};

const kenney = (name: string, category: string, ticked = true): ImportCandidate => ({
  name,
  ticked,
  preset: 'kenney',
  publisher: 'Kenney',
  target: `${category}/${name}`,
  targetError: null,
  targetFollowsPublisher: false,
  variants: [],
  steps: [
    { kind: 'extract_zip', enabled: true, options: { subpath: `Kenney/${category}/${name}` } },
    {
      kind: 'metadata',
      enabled: true,
      options: { preset: 'kenney', publisher: 'Kenney', lookup_store_page: true },
    },
  ],
  tree: tree(2),
});

const zipCandidate: ImportCandidate = {
  name: 'bundle',
  ticked: true,
  preset: 'none',
  publisher: 'Studio',
  target: 'bundle',
  targetError: null,
  targetFollowsPublisher: false,
  variants: [],
  steps: [
    { kind: 'extract_zip', enabled: true, options: {} },
    { kind: 'flatten', enabled: true, options: {} },
    { kind: 'metadata', enabled: true, options: { preset: 'none', publisher: 'Studio' } },
  ],
  tree: tree(9),
};

const fabScan: ImportScan = {
  originalPath: '/dl/Forest.zip',
  kind: 'zip',
  offers: [
    { layout: 'fab', candidates: [fab] },
    { layout: 'zip', candidates: [zipCandidate] },
  ],
  unavailable: {},
};

const kenneyScan: ImportScan = {
  originalPath: '/dl/Kenney.zip',
  kind: 'zip',
  offers: [
    {
      layout: 'kenney',
      candidates: [kenney('Car Kit', '3D assets'), kenney('Old Kit', 'Archive', false)],
    },
  ],
  unavailable: {},
};

describe('importRows', () => {
  const imports: ImportSummary[] = [
    {
      id: 'i1',
      libraryId: 'lib',
      originalPath: '/dl/Kenney.zip',
      kind: 'zip',
      createdAt: 1,
      packs: [
        { id: 'p2', target: '3D assets/Car Kit', preset: 'kenney', status: 'built', builtAt: 5 },
      ],
    },
    {
      id: 'i2',
      libraryId: 'lib',
      originalPath: '/dl/Kenney.zip',
      kind: 'zip',
      createdAt: 2,
      packs: [
        {
          id: 'p1',
          target: '2D assets/Tiles',
          preset: 'kenney',
          status: 'never_built',
          builtAt: null,
        },
      ],
    },
    {
      id: 'i3',
      libraryId: 'lib',
      originalPath: '/cache/Rocks.unitypackage',
      kind: 'unitypackage',
      createdAt: 3,
      packs: [{ id: 'p3', target: 'Pub/Rocks', preset: 'unity', status: 'failed', builtAt: null }],
    },
  ];

  test("gathers one original's imports in one row, each pack with its latest run", () => {
    const runs = [
      run('r3', ['p1'], { createdAt: 3 }),
      run('r2', ['p2'], { createdAt: 2, status: 'done' }),
      run('r1', ['p2'], { createdAt: 1, status: 'failed', error: 'old' }),
    ];
    const rows = importRows(imports, runs, { r3: progress('r3', 'hashing', 2, 5) });
    expect(rows.map((r) => [r.name, r.originalPath])).toEqual([
      ['Rocks.unitypackage', '/cache/Rocks.unitypackage'],
      ['Kenney.zip', '/dl/Kenney.zip'],
    ]);
    const kenneyRow = rows[1];
    expect(kenneyRow.packs.map((p) => [p.pack.target, p.run?.id, p.progress?.step])).toEqual([
      ['2D assets/Tiles', 'r3', 'hashing'],
      ['3D assets/Car Kit', 'r2', undefined],
    ]);
    expect(rows[0].packs[0].run).toBeNull();
    expect(kenneyRow.packs.map(packStatus)).toEqual(['Running', 'Built']);
    expect(packStatus(rows[0].packs[0])).toBe('Failed');
  });

  test('a cancelled or interrupted run shows as such unless the pack is built', () => {
    const pack = (status: 'built' | 'never_built') => ({
      id: 'p',
      target: 'Pub/Pack',
      preset: 'unity',
      status,
      builtAt: null,
    });
    for (const [runStatus, label] of [
      ['cancelled', 'Cancelled'],
      ['interrupted', 'Interrupted'],
    ] as const) {
      const r = run('r', ['p'], { status: runStatus });
      expect(packStatus({ pack: pack('never_built'), run: r, progress: null })).toBe(label);
      expect(packStatus({ pack: pack('built'), run: r, progress: null })).toBe('Built');
    }
  });

  test('keeps the same original apart in two libraries', () => {
    const other = { ...imports[2], id: 'i4', libraryId: 'other' };
    expect(importRows([imports[2], other], [], {}).map((r) => r.libraryId)).toEqual([
      'lib',
      'other',
    ]);
  });
});

describe('mergeRun', () => {
  test('keeps every held run, and an ended one over a stale listing of it live', () => {
    const held = [run('a', [], { status: 'done', createdAt: 2 }), run('b', [], { createdAt: 1 })];
    expect(mergeRun(held, run('a', [], { status: 'running', createdAt: 2 }))).toBe(held);
    const merged = mergeRun(held, run('b', [], { status: 'failed', createdAt: 1 }));
    expect(merged.map((r) => [r.id, r.status])).toEqual([
      ['a', 'done'],
      ['b', 'failed'],
    ]);
    expect(mergeRun(held, run('c', [], { createdAt: 3 })).map((r) => r.id)).toEqual([
      'c',
      'a',
      'b',
    ]);
  });
});

describe('progress and warnings', () => {
  test('a live run shows its step with counts, a queued one that it waits', () => {
    expect(progressText(run('r', []), progress('r', 'hashing', 3, 1200))).toBe('Hashing 3 / 1,200');
    expect(progressText(run('r', []), progress('r', 'swapping'))).toBe('Swapping');
    expect(progressText(run('r', [], { status: 'queued' }), null)).toBe(
      'Waiting for the run before it'
    );
  });

  test('a skipped entry is named by its path in the original', () => {
    expect(
      skippedText({ sourcePath: ['Inner.unitypackage', '../evil.txt'], reason: 'escapes' })
    ).toBe('Inner.unitypackage > ../evil.txt: climbs out of the pack');
  });
});

describe('Cancel', () => {
  test('works while a run is queued or staging, and reads Finishing once the build holds the lock', () => {
    const live = run('r', ['p']);
    const store = useImportStore.getState();
    const state = () => cancelState(live, useImportStore.getState().finishing);
    expect(cancelState(run('r', [], { status: 'queued' }), new Set())).toBe('cancel');
    for (const step of ['extracting', 'hashing', 'waiting for the library']) {
      store.applyProgress(progress('r', step));
      expect(state()).toBe('cancel');
    }
    // The build's marker right after the lock latches it, whatever steps follow
    store.applyProgress(progress('r', LOCKED_STEP));
    expect(state()).toBe('finishing');
    for (const step of ['swapping', 'indexing', 'a step added later', 'settling']) {
      store.applyProgress(progress('r', step));
      expect(state()).toBe('finishing');
    }
    expect(useImportStore.getState().finishing.has('other')).toBe(false);
    expect(cancelState(run('r', [], { status: 'done' }), new Set(['r']))).toBe('none');
  });

  test("a Cancel answered with Finishing marks the run, and the run's end clears it", async () => {
    const calls: string[] = [];
    api.cancelImportRun = async (libraryId, runId) => {
      calls.push(`${libraryId} ${runId}`);
      throw FINISHING;
    };
    api.listImports = async () => [];
    api.listImportRuns = async () => [];
    const live = run('r', ['p']);
    useImportStore.getState().applyUpdate(live);
    useImportStore.getState().applyProgress(progress('r', 'hashing', 1, 2));
    await useImportStore.getState().cancel(live);
    expect(calls).toEqual(['lib r']);
    let s = useImportStore.getState();
    expect(s.error).toBeNull();
    expect(cancelState(live, s.finishing)).toBe('finishing');

    useImportStore.getState().applyUpdate({ ...live, status: 'done' });
    s = useImportStore.getState();
    expect(s.finishing.has('r')).toBe(false);
    expect(s.progress.r).toBeUndefined();
    expect(s.runs.map((r) => r.status)).toEqual(['done']);
  });

  test('another refusal shows as the error', async () => {
    api.cancelImportRun = async () => {
      throw 'The run is neither queued nor running';
    };
    await useImportStore.getState().cancel(run('r', []));
    expect(useImportStore.getState().error).toBe('The run is neither queued nor running');
    expect(useImportStore.getState().finishing.size).toBe(0);
  });

  test("an imports answer older than the library's latest request is dropped", async () => {
    const pack = (status: 'built' | 'never_built') =>
      [
        {
          id: 'i',
          libraryId: 'lib',
          originalPath: '/o.unitypackage',
          kind: 'unitypackage',
          createdAt: 1,
          packs: [{ id: 'p', target: 'Pub/Pack', preset: 'unity', status, builtAt: null }],
        },
      ] as ImportSummary[];
    // The queued run's fetch reads before the build commits and answers last
    const answers: ((imports: ImportSummary[]) => void)[] = [];
    api.listImports = () => new Promise((resolve) => answers.push(resolve));
    api.listImportRuns = async () => [];
    const store = useImportStore.getState();
    const queuedFetch = store.fetchLibrary('lib');
    const endFetch = store.fetchLibrary('lib');
    answers[1](pack('built'));
    await endFetch;
    answers[0](pack('never_built'));
    await queuedFetch;
    expect(useImportStore.getState().imports.lib[0].packs[0].status).toBe('built');

    // Another library's fetch is its own
    api.listImports = async () => pack('never_built');
    await store.fetchLibrary('other');
    expect(useImportStore.getState().imports.other[0].packs[0].status).toBe('never_built');
  });

  test("a run's end refetches its library's imports, for its packs' status", async () => {
    const fetched: string[] = [];
    api.listImports = async (libraryId) => {
      fetched.push(libraryId);
      return [];
    };
    api.listImportRuns = async () => [];
    useImportStore.getState().applyUpdate(run('r', [], { status: 'running' }));
    expect(fetched).toEqual([]);
    useImportStore.getState().applyUpdate(run('r', [], { status: 'failed', error: 'clash' }));
    await Promise.resolve();
    expect(fetched).toEqual(['lib']);
  });
});

describe('Escape', () => {
  test('closes the Imports view only when the dialog is neither open nor took the key', () => {
    const key = (k: string, defaultPrevented = false) => ({ key: k, defaultPrevented });
    expect(escapeClosesPanel(key('Escape'), false)).toBe(true);
    // The panel's listener runs first: the dialog is still open
    expect(escapeClosesPanel(key('Escape'), true)).toBe(false);
    // The dialog's listener ran first and closed it, taking the key
    expect(escapeClosesPanel(key('Escape', true), false)).toBe(false);
    expect(escapeClosesPanel(key('Enter'), false)).toBe(false);
  });
});

describe('the draft', () => {
  test("starts from the scan's first offer, its URP variant ticked and its place steps read", () => {
    const draft = draftFromScan('lib', fabScan);
    expect(draft.offer).toBe(0);
    expect(draft.publisher).toBe('Studio');
    const c = draft.candidates[0];
    expect([c.variant, c.flatten, c.hoistOn, c.hoist, c.converts]).toEqual([
      1,
      true,
      true,
      'Art',
      true,
    ]);
    expect(c.scannedPlace).toEqual([{ kind: 'flatten' }, { kind: 'hoist', folder: 'Art' }]);
    expect(treeCurrent(c)).toBe(true);
    expect(draftProblem(draft)).toBeNull();
    expect(draftFromScan('lib', fabScan, 1).candidates[0].preset).toBe('none');
  });

  test("ticking another variant shows the scan's tree while the steps are the preset's", () => {
    let draft = pickVariant(draftFromScan('lib', fabScan), 0, 0);
    expect(draft.candidates[0].variants.map((v) => v.ticked)).toEqual([true, false]);
    expect(draft.candidates[0].tree?.files).toBe(3);
    expect(treeCurrent(draft.candidates[0])).toBe(true);

    // With hoist off the tree waits for a preview
    draft = patchCandidate(draft, 0, { hoistOn: false });
    expect(treeCurrent(draft.candidates[0])).toBe(false);
    expect(candidateProblem(draft, 0)).toBe('Previewing the steps…');
    draft = pickVariant(draft, 0, 1);
    expect(treeCurrent(draft.candidates[0])).toBe(false);
  });

  test("a variant's scanned tree is used only for the place steps the scan proposed", () => {
    // A scan whose Fab preset hoisted another folder: its variant trees are for that one
    const steps = fab.steps.map((s) =>
      s.kind === 'hoist' ? { ...s, options: { folder: 'Content' } } : s
    );
    let draft = draftFromScan('lib', {
      ...fabScan,
      offers: [{ layout: 'fab', candidates: [{ ...fab, steps }] }],
    });
    expect(draft.candidates[0].hoist).toBe('Content');
    draft = pickVariant(draft, 0, 0);
    expect([draft.candidates[0].tree?.files, treeCurrent(draft.candidates[0])]).toEqual([3, true]);
    // Hoisting Art instead, which isn't what the scan proposed, waits for a preview
    draft = patchCandidate(draft, 0, { hoist: 'Art' });
    draft = pickVariant(draft, 0, 1);
    expect(treeCurrent(draft.candidates[0])).toBe(false);
  });

  test("turning hoist off and on keeps its folder, and an emptied one takes the scan's again", () => {
    let draft = draftFromScan('lib', fabScan);
    draft = setHoist(draft, 0, false);
    expect(placeSteps(draft.candidates[0])).toEqual([{ kind: 'flatten' }]);
    draft = patchCandidate(setHoist(draft, 0, true), 0, { hoist: 'Meshes' });
    draft = setHoist(setHoist(draft, 0, false), 0, true);
    expect(draft.candidates[0].hoist).toBe('Meshes');
    draft = setHoist(patchCandidate(setHoist(draft, 0, false), 0, { hoist: ' ' }), 0, true);
    expect(draft.candidates[0].hoist).toBe('Art');
    // A pack whose scan proposed no hoist needs a folder named
    let zip = draftFromScan('lib', fabScan, 1);
    zip = setHoist(zip, 0, true);
    expect([zip.candidates[0].hoistOn, zip.candidates[0].hoist]).toEqual([true, '']);
    expect(candidateProblem(zip, 0)).toBe('Name the folder to move up');
  });

  test('a Fab pack with no variant ticked waits for one', () => {
    const untouched = {
      ...fab,
      variants: fab.variants.map((v) => ({ ...v, ticked: false })),
      tree: null,
    };
    const draft = draftFromScan('lib', {
      ...fabScan,
      offers: [{ layout: 'fab', candidates: [untouched] }],
    });
    expect(candidateProblem(draft, 0)).toBe('Tick the variant to import');
    expect(draftProblem(draft)).toBe('Forest: Tick the variant to import');
  });

  test("the publisher moves the targets you didn't type, never a Kenney pack's", () => {
    let draft = editPublisher(draftFromScan('lib', fabScan), 'Woods Inc');
    expect(draft.candidates[0].target).toBe('Woods Inc/Forest');
    draft = editTarget(draft, 0, 'Mine/Forest');
    draft = editPublisher(draft, 'Other');
    expect(draft.candidates[0].target).toBe('Mine/Forest');
    // A target that moved waits for the library's check
    expect(candidateProblem(draft, 0)).toBe('Checking the target…');

    const k = editPublisher(draftFromScan('lib', kenneyScan), 'Someone');
    expect(k.candidates.map((c) => c.target)).toEqual(['3D assets/Car Kit', 'Archive/Old Kit']);
    // Nor an "Any other zip" pack's, which the scan says stays <Pack>
    expect(editPublisher(draftFromScan('lib', fabScan, 1), 'Someone').candidates[0].target).toBe(
      'bundle'
    );
    expect(editPublisher(draftFromScan('lib', fabScan), '').candidates[0].target).toBe('Forest');
  });

  test("a refused target shows the library's reason, naming the other pack", () => {
    const refused = {
      ...fab,
      targetError: 'Studio/Forest would nest with the pack imported to Studio',
    };
    const draft = draftFromScan('lib', {
      ...fabScan,
      offers: [{ layout: 'fab', candidates: [refused] }],
    });
    expect(candidateProblem(draft, 0)).toBe(
      'Studio/Forest would nest with the pack imported to Studio'
    );
    expect(draftProblem(draft)).toContain('nest with the pack imported to Studio');
  });

  test('ticked candidates that nest with each other are refused naming the other', () => {
    let draft = draftFromScan('lib', kenneyScan);
    draft = patchCandidate(draft, 1, { ticked: true });
    expect(candidateProblem(draft, 1)).toBeNull();
    draft = editTarget(draft, 1, '3D assets');
    draft = patchCandidate(draft, 1, { checkedTarget: '3D assets', targetError: null });
    expect(candidateProblem(draft, 1)).toBe(
      '3D assets would nest with 3D assets/Car Kit, also ticked'
    );
    expect(candidateProblem(draft, 0)).toBe(
      '3D assets/Car Kit would nest with 3D assets, also ticked'
    );
    // Unticked, it no longer blocks
    draft = patchCandidate(draft, 1, { ticked: false });
    expect(draftProblem(draft)).toBeNull();
  });

  test('targets nest on whole path components only', () => {
    let draft = draftFromScan('lib', kenneyScan);
    draft = patchCandidate(draft, 1, { ticked: true });
    const at = (d: typeof draft, i: number, target: string) =>
      patchCandidate(editTarget(d, i, target), i, { checkedTarget: target, targetError: null });
    draft = at(at(draft, 0, 'Pub/Pack'), 1, 'Pub/Package');
    expect([candidateProblem(draft, 0), candidateProblem(draft, 1)]).toEqual([null, null]);
    expect(draftProblem(draft)).toBeNull();
    draft = at(draft, 1, 'Pub/Pack/Sub');
    expect(candidateProblem(draft, 0)).toBe('Pub/Pack would nest with Pub/Pack/Sub, also ticked');
    expect(candidateProblem(draft, 1)).toBe('Pub/Pack/Sub would nest with Pub/Pack, also ticked');
  });

  test('Import waits for a ticked pack and a publisher, and a build that would fail', () => {
    let draft = draftFromScan('lib', kenneyScan);
    expect(draftProblem(patchCandidate(draft, 0, { ticked: false }))).toBe('Tick a pack to import');
    expect(draftProblem({ ...draft, publisher: ' ' })).toBe('Name the publisher');
    draft = patchCandidate(draft, 0, { tree: tree(0, 'two files at Rock.png') });
    expect(candidateProblem(draft, 0)).toBe('The build would fail: two files at Rock.png');
  });

  test("each candidate's request names its source, place steps and metadata", () => {
    let draft = draftFromScan('lib', fabScan);
    draft = patchCandidate(draft, 0, {
      title: ' Forest Pack ',
      version: '1.2',
      license: 'Standard EULA',
    });
    expect(importRequest(draft, draft.candidates[0])).toEqual({
      libraryId: 'lib',
      originalPath: '/dl/Forest.zip',
      target: 'Studio/Forest',
      publisher: 'Studio',
      preset: 'fab',
      variant: 'Forest_URP.unitypackage',
      place: [{ kind: 'flatten' }, { kind: 'hoist', folder: 'Art' }],
      title: 'Forest Pack',
      version: '1.2',
      license: 'Standard EULA',
    });

    const k = patchCandidate(draftFromScan('lib', kenneyScan), 0, { lookupStorePage: false });
    expect(importRequest(k, k.candidates[0])).toEqual({
      libraryId: 'lib',
      originalPath: '/dl/Kenney.zip',
      target: '3D assets/Car Kit',
      publisher: 'Kenney',
      preset: 'kenney',
      subpath: 'Kenney/3D assets/Car Kit',
      place: [],
      lookupStorePage: false,
    });
  });
});

/** Waits, a microtask at a time, until `done` holds */
async function waitUntil(done: () => boolean) {
  for (let i = 0; i < 100 && !done(); i++) await new Promise((r) => setTimeout(r, 0));
  expect(done()).toBe(true);
}

describe('the dialog', () => {
  /** Opens the dialog over `scan`, answered by the stand-in API */
  async function open(scan: ImportScan) {
    api.scanImport = async () => scan;
    api.listImports = async () => [];
    api.listImportRuns = async () => [];
    await useImportStore.getState().openDialog(scan.originalPath, 'lib');
  }

  test('scans the original for the library, and a scan error shows', async () => {
    await open(fabScan);
    expect(useImportStore.getState().draft?.candidates[0].name).toBe('Forest');
    api.scanImport = async () => {
      throw 'The original /x.zip is inside the library: an original stays outside it';
    };
    await useImportStore.getState().setLibrary('other');
    const s = useImportStore.getState();
    expect(s.draft).toBeNull();
    expect(s.scanError).toContain('inside the library');
    expect(s.dialog).toEqual({ originalPath: '/dl/Forest.zip', libraryId: 'other' });
  });

  test('an edited target is checked by the library, and a late answer for an older value is dropped', async () => {
    await open(fabScan);
    let answer: (error: string | null) => void = () => {};
    api.checkImportTarget = (_lib, target) =>
      new Promise((resolve) => {
        answer = (error) => resolve(target === 'Pub' ? error : null);
      });
    const store = useImportStore.getState();
    store.edit((d) => editTarget(d, 0, 'Pub'));
    const first = store.checkTarget(0);
    store.edit((d) => editTarget(d, 0, 'Pub/Pack/Sub'));
    answer('Pub would nest with the pack imported to Pub/Pack');
    await first;
    // The answer for 'Pub' came after the target moved on, so the scan's check stands
    expect(useImportStore.getState().draft?.candidates[0].checkedTarget).toBe('Studio/Forest');

    api.checkImportTarget = async (_lib, target) =>
      `${target} would nest with the pack imported to Pub/Pack`;
    await store.checkTarget(0);
    const c = useImportStore.getState().draft!.candidates[0];
    expect([c.checkedTarget, c.targetError]).toEqual([
      'Pub/Pack/Sub',
      'Pub/Pack/Sub would nest with the pack imported to Pub/Pack',
    ]);
    expect(candidateProblem(useImportStore.getState().draft!, 0)).toContain('Pub/Pack');
  });

  test("the scan's reason a step can't run here stays with the draft across layouts", async () => {
    const why = 'Blender 5.2 is needed to convert models; set it in Settings';
    await open({ ...fabScan, unavailable: { unity_glb: why } });
    expect(useImportStore.getState().draft!.unavailable).toEqual({ unity_glb: why });
    useImportStore.getState().pickOffer(1);
    useImportStore.getState().pickOffer(0);
    const draft = useImportStore.getState().draft!;
    expect([draft.offer, draft.unavailable.unity_glb, draft.candidates[0].converts]).toEqual([
      0,
      why,
      true,
    ]);
  });

  test('picking another layout drafts its candidates', async () => {
    await open(fabScan);
    useImportStore.getState().pickOffer(1);
    const draft = useImportStore.getState().draft!;
    expect(draft.offer).toBe(1);
    expect(draft.candidates.map((c) => [c.name, c.preset, c.target])).toEqual([
      ['bundle', 'none', 'bundle'],
    ]);
    expect(draftProblem(draft)).toBeNull();
  });

  test('a target check that fails shows its error, and an empty target asks for a folder', async () => {
    await open(fabScan);
    const store = useImportStore.getState();
    api.checkImportTarget = async () => {
      throw "Library lib isn't open";
    };
    store.edit((d) => editTarget(d, 0, 'Pub/Forest'));
    await store.checkTarget(0);
    let c = useImportStore.getState().draft!.candidates[0];
    expect([c.checkedTarget, c.targetError]).toEqual(['Pub/Forest', "Library lib isn't open"]);
    expect(candidateProblem(useImportStore.getState().draft!, 0)).toBe("Library lib isn't open");

    let asked = 0;
    api.checkImportTarget = async () => {
      asked++;
      return null;
    };
    store.edit((d) => editTarget(d, 0, ''));
    await store.checkTarget(0);
    c = useImportStore.getState().draft!.candidates[0];
    expect([asked, c.checkedTarget, c.targetError]).toEqual([0, '', null]);
    expect(candidateProblem(useImportStore.getState().draft!, 0)).toBe(
      'Name the folder to import into'
    );
  });

  test('a preview that fails blocks Import with its reason', async () => {
    await open(fabScan);
    api.previewImport = async () => {
      throw "The original /dl/Forest.zip can't be read: gone";
    };
    const store = useImportStore.getState();
    store.edit((d) => patchCandidate(d, 0, { hoistOn: false }));
    await store.refreshTree(0);
    const draft = useImportStore.getState().draft!;
    expect(treeCurrent(draft.candidates[0])).toBe(true);
    expect(candidateProblem(draft, 0)).toBe(
      "The build would fail: The original /dl/Forest.zip can't be read: gone"
    );
    expect(draftProblem(draft)).toContain('The build would fail');
  });

  test("the dialog stays open while Import submits, so a later pack's refusal shows", async () => {
    await open(kenneyScan);
    const store = useImportStore.getState();
    store.edit((d) => patchCandidate(d, 1, { ticked: true }));
    let refuse: () => void = () => {};
    api.startImport = (request) =>
      request.target === 'Archive/Old Kit'
        ? new Promise((_, reject) => {
            refuse = () => reject('Archive/Old Kit would nest with the pack imported to Archive');
          })
        : Promise.resolve(run('r1', ['p1'], { status: 'queued', createdAt: 1 }));
    const submitted = store.submit();
    await waitUntil(() => useImportStore.getState().draft!.candidates[0].imported);
    // Escape, Close and the overlay all close through closeDialog
    useImportStore.getState().closeDialog();
    expect(useImportStore.getState().dialog).not.toBeNull();
    refuse();
    expect(await submitted).toBe(false);
    const s = useImportStore.getState();
    expect(s.dialog).not.toBeNull();
    expect(s.draft!.candidates[1].error).toBe(
      'Archive/Old Kit would nest with the pack imported to Archive'
    );
    // Once Import is done, it closes
    s.closeDialog();
    expect(useImportStore.getState().dialog).toBeNull();
  });

  test('edits made while Import submits change nothing, so each pack goes as it was', async () => {
    await open(kenneyScan);
    const store = useImportStore.getState();
    store.edit((d) => patchCandidate(d, 1, { ticked: true }));
    const requests: ImportRequest[] = [];
    let answerFirst: () => void = () => {};
    api.startImport = (request) => {
      requests.push(request);
      const queued = run(`r${requests.length}`, ['p'], { status: 'queued' });
      return requests.length === 1
        ? new Promise((resolve) => (answerFirst = () => resolve(queued)))
        : Promise.resolve(queued);
    };
    const submitted = store.submit();
    await waitUntil(() => requests.length === 1);
    // Untick the pending pack, retarget it, and Untick all, while the first answer is pending
    const before = useImportStore.getState().draft;
    store.edit((d) => editTarget(d, 1, 'Vehicles/Old Kit'));
    store.edit((d) => patchCandidate(d, 1, { ticked: false }));
    store.edit((d) => ({ ...d, candidates: d.candidates.map((c) => ({ ...c, ticked: false })) }));
    expect(useImportStore.getState().draft).toBe(before);
    answerFirst();
    expect(await submitted).toBe(true);
    expect(requests.map((r) => r.target)).toEqual(['3D assets/Car Kit', 'Archive/Old Kit']);
    expect(useImportStore.getState().dialog).toBeNull();
  });

  test('a step edit previews the tree of the new steps', async () => {
    await open(fabScan);
    const calls: unknown[] = [];
    api.previewImport = async (libraryId, originalPath, variant, subpath, place) => {
      calls.push({ libraryId, originalPath, variant, subpath, place });
      return tree(7);
    };
    const store = useImportStore.getState();
    await store.refreshTree(0);
    expect(calls).toEqual([]);
    store.edit((d) => patchCandidate(d, 0, { hoistOn: false }));
    await store.refreshTree(0);
    expect(calls).toEqual([
      {
        libraryId: 'lib',
        originalPath: '/dl/Forest.zip',
        variant: 'Forest_URP.unitypackage',
        subpath: null,
        place: [{ kind: 'flatten' }],
      },
    ]);
    const c = useImportStore.getState().draft!.candidates[0];
    expect([c.tree?.files, treeCurrent(c)]).toEqual([7, true]);
  });

  test('Import queues each ticked pack and keeps a refused one in the dialog with its reason', async () => {
    await open(kenneyScan);
    const store = useImportStore.getState();
    store.edit((d) => patchCandidate(d, 1, { ticked: true }));
    const requests: ImportRequest[] = [];
    api.startImport = async (request) => {
      requests.push(request);
      if (request.target === 'Archive/Old Kit')
        throw 'Archive/Old Kit would nest with the pack imported to Archive';
      return run('r1', ['p1'], { status: 'queued', createdAt: 1 });
    };
    expect(await store.submit()).toBe(false);
    expect(requests.map((r) => r.target)).toEqual(['3D assets/Car Kit', 'Archive/Old Kit']);
    let s = useImportStore.getState();
    expect(s.dialog).not.toBeNull();
    expect(s.draft!.candidates.map((c) => [c.imported, c.error])).toEqual([
      [true, null],
      [false, 'Archive/Old Kit would nest with the pack imported to Archive'],
    ]);
    expect(s.runs.map((r) => r.id)).toEqual(['r1']);

    // Another target goes through, and only the pack not yet imported is sent
    s.edit((d) => editTarget(d, 1, 'Old/Old Kit'));
    api.checkImportTarget = async () => null;
    await s.checkTarget(1);
    requests.length = 0;
    api.startImport = async (request) => {
      requests.push(request);
      return run('r2', ['p2'], { status: 'queued', createdAt: 2 });
    };
    expect(await useImportStore.getState().submit()).toBe(true);
    expect(requests.map((r) => r.target)).toEqual(['Old/Old Kit']);
    s = useImportStore.getState();
    expect(s.dialog).toBeNull();
    expect(s.runs.map((r) => r.id)).toEqual(['r2', 'r1']);
  });
});
