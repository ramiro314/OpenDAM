import { describe, expect, test } from 'bun:test';
import {
  domainsLine,
  exportedView,
  facingLine,
  hasGameTab,
  isPascal,
  namePatch,
  rigPatch,
  shownValue,
  turnPatch,
  viewKey,
} from './gameTab';

const inferred = { inferred: 'Crate', yours: null };
const yours = { inferred: 'Crate', yours: 'WoodenCrate' };

describe('shownValue', () => {
  test('shows yours when set, else the inferred value, marked so', () => {
    expect(shownValue(inferred)).toEqual({ value: 'Crate', mark: 'inferred' });
    expect(shownValue(yours)).toEqual({ value: 'WoodenCrate', mark: 'yours' });
  });
});

describe('namePatch', () => {
  test('writes a new PascalCase name, trimmed', () => {
    expect(namePatch(inferred, ' WoodenCrate ')).toEqual({ patch: { assetName: 'WoodenCrate' } });
    expect(namePatch(yours, 'OakCrate')).toEqual({ patch: { assetName: 'OakCrate' } });
  });

  test('changes nothing when the name shown is kept', () => {
    expect(namePatch(inferred, 'Crate')).toBeNull();
    expect(namePatch(yours, 'WoodenCrate ')).toBeNull();
  });

  test('resets yours when the inferred name is typed back', () => {
    expect(namePatch(yours, 'Crate')).toEqual({ patch: { assetName: null } });
  });

  test('refuses an empty name or one that isn’t PascalCase', () => {
    expect(namePatch(yours, '  ')).toEqual({
      error: 'A game name can’t be empty: Reset goes back to the inferred one',
    });
    expect(namePatch(inferred, 'wooden crate')).toEqual({
      error:
        'wooden crate isn’t PascalCase: a game name starts with a capital letter, then only letters and digits',
    });
  });
});

describe('isPascal', () => {
  test('is the contract’s pascalName', () => {
    for (const name of ['Crate', 'A', 'Crate01', 'RGBMask']) expect(isPascal(name)).toBe(true);
    for (const name of ['', 'crate', '1Crate', 'Wooden_Crate', 'Café']) {
      expect(isPascal(name)).toBe(false);
    }
  });
});

describe('hasGameTab', () => {
  test('shows for models and images only', () => {
    expect(['3d_model', 'texture', 'image', 'hdri'].every((k) => hasGameTab(k as never))).toBe(
      true
    );
    expect(['document', 'video', 'material', 'other'].some((k) => hasGameTab(k as never))).toBe(
      false
    );
  });
});

describe('domainsLine', () => {
  test('names the domains, or says there is none', () => {
    expect(domainsLine({ domains: [] })).toBe(
      'In no domain yet: add it to a collection inside the root collection'
    );
    expect(domainsLine({ domains: ['Props'] })).toBe('In the domain Props');
    expect(domainsLine({ domains: ['Loot', 'Props'] })).toBe('In the domains Loot, Props');
  });
});

describe('rigPatch', () => {
  const inferredNone = { inferred: 'none' as const, yours: null };
  test('writes a rig other than the inferred one', () => {
    expect(rigPatch(inferredNone, 'generic')).toEqual({ rig: 'generic' });
  });

  test('resets yours when the inferred rig is chosen back, and changes nothing when kept', () => {
    expect(rigPatch({ inferred: 'generic', yours: 'none' }, 'generic')).toEqual({ rig: null });
    expect(rigPatch(inferredNone, 'none')).toBeNull();
    expect(rigPatch({ inferred: 'generic', yours: 'none' }, 'none')).toBeNull();
  });
});

describe('turnPatch', () => {
  const unturned = { inferred: 0, yours: null };
  test('turns a quarter clockwise or back, wrapping from 3 to 0', () => {
    expect(turnPatch(unturned, 1)).toEqual({ facingQuarterTurns: 1 });
    expect(turnPatch(unturned, -1)).toEqual({ facingQuarterTurns: 3 });
    expect(turnPatch({ inferred: 0, yours: 2 }, 1)).toEqual({ facingQuarterTurns: 3 });
  });

  test('resets yours once it is back at the origin’s default', () => {
    expect(turnPatch({ inferred: 0, yours: 3 }, 1)).toEqual({ facingQuarterTurns: null });
    expect(turnPatch({ inferred: 0, yours: 1 }, -1)).toEqual({ facingQuarterTurns: null });
  });
});

describe('facingLine', () => {
  test('says the origin’s default, then your turns', () => {
    const turns = (yours: number | null) => ({ inferred: 0, yours });
    expect(facingLine({ origin: 'unreal', facingQuarterTurns: turns(null) })).toBe(
      'Its pack is from Unreal, so its +X is turned to face +Z'
    );
    expect(facingLine({ origin: 'generic', facingQuarterTurns: turns(1) })).toBe(
      'Its pack’s origin is generic, so it faces as its file does, then turned 90° clockwise seen from above'
    );
    expect(facingLine({ origin: null, facingQuarterTurns: turns(3) })).toBe(
      'It is in no pack yet, then turned 270° clockwise seen from above'
    );
  });
});

describe('exportedView', () => {
  test('is hidden until you ask, then loads until the answer is for the current revision', () => {
    const key = viewKey('a', 0);
    expect(exportedView(false, null, key)).toEqual({ status: 'hidden' });
    expect(exportedView(true, null, key)).toEqual({ status: 'loading' });
    const shown = { key, path: '/lib/.opendam/export-cache/models/k.glb' };
    expect(exportedView(true, shown, key)).toEqual({ status: 'shown', path: shown.path });
    expect(exportedView(true, { key, error: 'No Blender' }, key)).toEqual({
      status: 'failed',
      error: 'No Blender',
    });
    // After an edit, such as a quarter turn, the view asks again
    const next = viewKey('a', 1);
    expect(next).not.toBe(key);
    expect(exportedView(true, shown, next)).toEqual({ status: 'loading' });
    expect(viewKey('b', 0)).not.toBe(key);
  });
});
