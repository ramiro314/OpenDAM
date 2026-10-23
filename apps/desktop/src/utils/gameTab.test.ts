import { describe, expect, test } from 'bun:test';
import type { GameImage, GameMaterial, GameRoleTexture, TextureRole } from '../types';
import {
  domainsLine,
  hasGameTab,
  isPascal,
  materialNameError,
  namePatch,
  pickerOptions,
  roleEditable,
  shortPaths,
  shownValue,
  slotPatch,
  sourcePatch,
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

const image = (assetId: string, filePath: string, roles: TextureRole[]): GameImage => ({
  assetId,
  filePath,
  layout: roles.length === 1 ? roles[0] : 'ORM',
  roles,
});

describe('pickerOptions', () => {
  const images = [
    image('n1k', 'Pack/Textures/1K/T_Crate_N.png', ['Normal']),
    image('n4k', 'Pack/Textures/4K/T_Crate_N.png', ['Normal']),
    image('d', 'Pack/Textures/T_Crate_D.png', ['BaseColor']),
    image('orm', 'Pack/Textures/T_Crate_ORM.png', ['Occlusion', 'MetallicSmoothness']),
  ];

  test('offers the pack’s images that give the role', () => {
    expect(pickerOptions(images, 'Normal', null).map((i) => i.assetId)).toEqual(['n1k', 'n4k']);
    expect(pickerOptions(images, 'Occlusion', null).map((i) => i.assetId)).toEqual(['orm']);
    expect(pickerOptions(images, 'Height', null)).toEqual([]);
  });

  test('keeps the image the role uses now, in path order', () => {
    expect(pickerOptions(images, 'Normal', 'd').map((i) => i.assetId)).toEqual(['n1k', 'n4k', 'd']);
  });
});

describe('shortPaths', () => {
  test('drops the folders every image shares', () => {
    const paths = shortPaths([
      image('n1k', 'Pack/Textures/1K/T_Crate_N.png', ['Normal']),
      image('n4k', 'Pack/Textures/4K/T_Crate_N.png', ['Normal']),
      image('d', 'Pack/Textures/T_Crate_D.png', ['BaseColor']),
    ]);
    expect([...paths.values()]).toEqual(['1K/T_Crate_N.png', '4K/T_Crate_N.png', 'T_Crate_D.png']);
    expect([...shortPaths([image('a', 'Pack/a.png', [])]).values()]).toEqual(['a.png']);
    expect(shortPaths([])).toEqual(new Map());
  });
});

describe('roleEditable', () => {
  test('a role is yours to pick only when the layout passes one role through', () => {
    expect(roleEditable('passThrough')).toBe(true);
    for (const layout of ['ORM', 'MaskMap', 'Roughness', 'Gloss', 'Metallic'] as const) {
      expect(roleEditable(layout)).toBe(false);
    }
  });
});

const pick = (assetId: string) => ({ assetId, assetName: 'Crate', files: [assetId] });
/** A role inferred as `inferred`, and when `yours` is given, yours: an image's id or null for none */
const role = (r: TextureRole, inferred: string | null, yours?: string | null): GameRoleTexture => {
  const shown = yours === undefined ? inferred : yours;
  return {
    role: r,
    inferred: inferred === null ? null : pick(inferred),
    yours: yours !== undefined,
    value: shown === null ? null : pick(shown),
  };
};
const planks = (textures: GameRoleTexture[], name: string | null = null): GameMaterial => ({
  slot: 'Planks',
  added: false,
  name: { inferred: 'Planks', yours: name },
  surface: { inferred: 'opaque', yours: null },
  cutoutSuggested: true,
  textures,
});

describe('slotPatch', () => {
  test('writes the slot’s edits whole, keeping the others', () => {
    const m = planks([role('BaseColor', 'd', null), role('Normal', 'n1k')], 'Boards');
    expect(slotPatch(m, { field: 'texture', role: 'Normal', value: 'n4k' })).toEqual({
      materials: { Planks: { name: 'Boards', textures: { BaseColor: null, Normal: 'n4k' } } },
    });
    expect(slotPatch(m, { field: 'surface', value: 'cutout' })).toEqual({
      materials: { Planks: { name: 'Boards', surface: 'cutout', textures: { BaseColor: null } } },
    });
  });

  test('picking none is stored as null, and the inferred value resets yours', () => {
    const m = planks([role('BaseColor', 'd'), role('Normal', 'n1k', 'n4k')]);
    expect(slotPatch(m, { field: 'texture', role: 'BaseColor', value: null })).toEqual({
      materials: { Planks: { textures: { BaseColor: null, Normal: 'n4k' } } },
    });
    expect(slotPatch(m, { field: 'texture', role: 'Normal', value: 'n1k' })).toEqual({
      materials: { Planks: null },
    });
    expect(slotPatch(m, { field: 'texture', role: 'Normal', value: undefined })).toEqual({
      materials: { Planks: null },
    });
    expect(slotPatch(planks([], 'Boards'), { field: 'name', value: 'Planks' })).toEqual({
      materials: { Planks: null },
    });
  });
});

describe('sourcePatch', () => {
  test('sets a source’s value, or resets it when it is the inferred one', () => {
    const layout = { inferred: 'Roughness' as const, yours: null };
    expect(sourcePatch('layout', layout, 'passThrough')).toEqual({ layout: 'passThrough' });
    expect(sourcePatch('layout', layout, 'Roughness')).toEqual({ layout: null });
    const convention = { inferred: 'directx' as const, yours: 'opengl' as const };
    expect(sourcePatch('normalConvention', convention, 'directx')).toEqual({
      normalConvention: null,
    });
  });
});

describe('materialNameError', () => {
  test('refuses an empty name or one that isn’t PascalCase', () => {
    expect(materialNameError('Boards')).toBeNull();
    expect(materialNameError('')).toMatch(/can’t be empty/);
    expect(materialNameError('old boards')).toMatch(/isn’t PascalCase/);
  });
});
