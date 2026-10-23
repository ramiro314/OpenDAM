import { describe, expect, test } from 'bun:test';
import { domainsLine, hasGameTab, isPascal, namePatch, shownValue } from './gameTab';

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
