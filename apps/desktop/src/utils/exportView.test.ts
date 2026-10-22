import { describe, expect, test } from 'bun:test';
import type { ExportCheck, ExportCheckAsset } from '../types';
import { checkSummary, exportMenuReason } from './exportView';

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
});

const check = (statuses: ExportCheckAsset['status'][], problems: number): ExportCheck => ({
  domain: 'Props',
  bundlePath: '/export/DungeonGame/Props',
  assets: statuses.map(asset),
  packs: [],
  problems: Array.from({ length: problems }, (_, i) => ({ message: `p${i}`, assets: [] })),
  manifest: {},
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
});
