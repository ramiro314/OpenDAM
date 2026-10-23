import { describe, expect, test } from 'bun:test';
import type { ExportCheck, ExportCheckAsset, ExportDiff } from '../types';
import {
  checkSummary,
  diffLabel,
  diffSummary,
  editedAsset,
  editedMessage,
  exportMenuReason,
  materialLine,
  staleMessage,
} from './exportView';

const settings = { rootCollection: 'dg' };
const collections = [{ id: 'dg', name: 'DungeonGame' }];

describe('exportMenuReason', () => {
  test('a direct, ordinary child of the root collection is a domain', () => {
    expect(exportMenuReason({ parentId: 'dg', isSmart: false }, settings, collections)).toBeNull();
  });

  test('anything else says why it is not', () => {
    expect(exportMenuReason({ parentId: 'dg', isSmart: false }, null, collections)).toBe(
      'Loading the export settings…'
    );
    expect(exportMenuReason({ parentId: 'dg', isSmart: false }, { rootCollection: null }, [])).toBe(
      'Set a root collection in Settings, under Game export'
    );
    expect(exportMenuReason({ parentId: 'dg', isSmart: true }, settings, collections)).toBe(
      'A smart collection can’t be a domain'
    );
    expect(exportMenuReason({ parentId: undefined, isSmart: false }, settings, collections)).toBe(
      'Only collections directly inside DungeonGame export'
    );
    expect(exportMenuReason({ parentId: 'other', isSmart: false }, settings, [])).toBe(
      'Only collections directly inside the root collection export'
    );
  });
});

const asset = (status: ExportCheckAsset['status']): ExportCheckAsset => ({
  libraryId: 'lib',
  assetId: `${status}-${Math.random()}`,
  filePath: 'Pack/T_Rock_D.png',
  kind: 'texture',
  assetName: 'Rock',
  role: 'BaseColor',
  pack: 'pack',
  status,
  member: true,
  layout: 'BaseColor',
});

const check = (statuses: ExportCheckAsset['status'][], problems: number): ExportCheck => ({
  domain: 'Props',
  bundlePath: '/export/DungeonGame/Props',
  assets: statuses.map(asset),
  packs: [],
  problems: Array.from({ length: problems }, (_, i) => ({ message: `p${i}`, assets: [] })),
  manifest: {},
  diff: null,
  staged: false,
});

describe('checkSummary', () => {
  test('counts ready assets and the problems that refuse the export', () => {
    expect(checkSummary(check([], 0))).toBe('The domain holds no assets yet');
    expect(checkSummary(check(['ready'], 0))).toBe('All 1 asset ready. Nothing refuses the export');
    expect(checkSummary(check(['ready', 'problem', 'problem'], 2))).toBe(
      '1 of 3 assets ready. 2 problems refuse the export'
    );
    expect(checkSummary(check([], 1))).toBe('No assets. 1 problem refuses the export');
  });

  test('new and changed assets are ready to export', () => {
    expect(checkSummary(check(['new', 'changed', 'ready'], 0))).toBe(
      'All 3 assets ready. Nothing refuses the export'
    );
  });
});

const rock = { kind: 'texture', assetName: 'Rock', role: 'BaseColor' } as const;
const crate = { kind: 'model', assetName: 'Crate', role: null } as const;
const diff = (previousExportedAt: string | null, parts: Partial<ExportDiff>): ExportDiff => ({
  previousExportedAt,
  added: [],
  changed: [],
  removed: [],
  packs: [],
  ...parts,
});

describe('diffSummary', () => {
  test('a first export adds everything', () => {
    expect(diffSummary(diff(null, { added: [rock, crate] }))).toBe(
      'No bundle yet: the export adds 2 assets'
    );
    expect(diffSummary(diff(null, {}))).toBe('No bundle yet, and nothing to export');
  });

  test('a later export counts what it adds, changes and removes', () => {
    const then = '2026-10-22T12:00:00+02:00';
    expect(diffSummary(diff(then, {}))).toBe('The bundle already holds exactly this');
    expect(diffSummary(diff(then, { added: [rock], removed: [crate, rock] }))).toBe(
      'Against the previous bundle: 1 added, 2 removed'
    );
    expect(diffSummary(diff(then, { changed: [rock] }))).toBe(
      'Against the previous bundle: 1 changed'
    );
    // A pack-only change is a change too
    expect(diffSummary(diff(then, { packs: ['kenney-crates'] }))).toBe(
      'Against the previous bundle: 1 pack changed'
    );
  });

  test('labels name a texture with its role', () => {
    expect(diffLabel(rock)).toBe('Rock (BaseColor)');
    expect(diffLabel(crate)).toBe('Crate');
  });
});

describe('staleMessage', () => {
  test('names the first three changes and counts the rest', () => {
    expect(staleMessage(["the domain's name"])).toBe(
      "Nothing was exported: since the Check, this changed: the domain's name. Check again"
    );
    expect(staleMessage(['a', 'b', 'c', 'd', 'e'])).toBe(
      'Nothing was exported: since the Check, this changed: a; b; c and 2 more. Check again'
    );
  });
});

describe('editedAsset', () => {
  const listed = [
    { ...asset('ready'), assetId: 'crate', filePath: 'Kit/crate.png' },
    { ...asset('problem'), assetId: 'rock', filePath: 'Kit/T_Rock_D.png' },
  ];

  test('finds the checked asset an edit wrote, which makes the Check stale', () => {
    // An image's name reaches its texture set, so the ids hold sources the Check doesn't list
    const found = editedAsset({ assets: listed }, ['rock-normal', 'rock']);
    expect(found?.assetId).toBe('rock');
    expect(editedMessage(found!)).toBe(
      'You edited Kit/T_Rock_D.png since the Check. Check again to export it'
    );
  });

  test('an edit to an asset the Check doesn’t hold leaves it as it is', () => {
    expect(editedAsset({ assets: listed }, ['elsewhere'])).toBeNull();
  });
});

describe('materialLine', () => {
  test('names the slot, its textures and where each was found', () => {
    expect(
      materialLine({
        slot: 'M_Stone',
        name: 'Stone',
        surface: 'opaque',
        cutoutSuggested: false,
        added: false,
        textures: {
          BaseColor: { via: 'fbx', assetName: 'Pillar', files: ['Kit/T_Stone_D.png'] },
          Normal: { via: 'suffix', assetName: 'Pillar', files: ['Kit/T_Stone_N.png'] },
        },
      })
    ).toBe(
      'M_Stone → Stone, opaque: BaseColor Pillar (from the FBX), Normal Pillar (by file name)'
    );
  });

  test('marks a cutout suggestion and the slot the export adds', () => {
    expect(
      materialLine({
        slot: 'OpenDAMDefault',
        name: 'OpenDAMDefault',
        surface: 'opaque',
        cutoutSuggested: true,
        added: true,
        textures: {},
      })
    ).toBe(
      'OpenDAMDefault → OpenDAMDefault, opaque (cutout suggested), added at export: no textures'
    );
  });
});
