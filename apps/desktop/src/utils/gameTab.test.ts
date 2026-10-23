import { describe, expect, test } from 'bun:test';
import type { GameImage, GameMaterial, GameRoleTexture, TextureRole } from '../types';
import {
  domainsLine,
  exportedView,
  facingLine,
  hasGameTab,
  isPascal,
  materialNameError,
  namePatch,
  pickerOptions,
  rigPatch,
  roleEditable,
  shortPaths,
  shownValue,
  slotPatch,
  sourcePatch,
  turnPatch,
  viewKey,
  withPatch,
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
    picked: yours ?? null,
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

describe('slotPatch with a pick the export can’t use', () => {
  test('keeps your pick when another field of the slot changes', () => {
    // Normal's picked image left the pack: the export binds nothing, but the pick is yours
    const stale: GameRoleTexture = { ...role('Normal', 'n1k', 'n4k'), value: null };
    const m = planks([role('BaseColor', 'd'), stale]);
    expect(slotPatch(m, { field: 'surface', value: 'cutout' })).toEqual({
      materials: { Planks: { surface: 'cutout', textures: { Normal: 'n4k' } } },
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

describe('withPatch', () => {
  const model = {
    rig: { inferred: 'none' as const, yours: null },
    facingQuarterTurns: { inferred: 0, yours: null },
    origin: 'generic' as const,
  };

  test('shows the saved rig and turns as yours, and a reset as inferred', () => {
    const saved = withPatch(model, { rig: 'generic', facingQuarterTurns: 2 });
    expect(saved.rig).toEqual({ inferred: 'none', yours: 'generic' });
    expect(saved.facingQuarterTurns).toEqual({ inferred: 0, yours: 2 });
    expect(withPatch(saved, { facingQuarterTurns: null }).facingQuarterTurns).toEqual({
      inferred: 0,
      yours: null,
    });
    expect(withPatch(model, null)).toBe(model);
  });

  test('a second turn before the detail is read back builds on the first', () => {
    const first = turnPatch(model.facingQuarterTurns, 1);
    const shown = withPatch(model, first);
    expect(turnPatch(shown.facingQuarterTurns, 1)).toEqual({ facingQuarterTurns: 2 });
    // From 3, one turn resets, and the next turns from the default again
    const three = withPatch(model, { facingQuarterTurns: 3 });
    const reset = withPatch(three, turnPatch(three.facingQuarterTurns, 1));
    expect(turnPatch(reset.facingQuarterTurns, 1)).toEqual({ facingQuarterTurns: 1 });
  });
});
