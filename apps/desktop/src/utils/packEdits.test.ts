import { describe, expect, test } from 'bun:test';
import type { PackDetail, PackOverride } from '../types';
import { detailsEdit, markChoices, packForm, sourceUrlProblem } from './packEdits';

const pack = (name: string, edits: PackOverride = {}): PackDetail => ({
  libraryId: 'lib',
  path: '3D assets/Car Kit',
  name,
  parentPath: '3D assets',
  itemCount: 1,
  hiddenCount: 0,
  gallery: [],
  kinds: {},
  sizeBytes: 0,
  edits,
});

describe('the pack mark items', () => {
  test('a pack offers "Not a pack", any other folder "Mark as pack"', () => {
    expect(markChoices(true, null).map((c) => [c.label, c.mark])).toEqual([
      ['Not a pack', 'not_pack'],
    ]);
    expect(markChoices(false, undefined).map((c) => [c.label, c.mark])).toEqual([
      ['Mark as pack', 'pack'],
    ]);
  });

  test('a folder you marked either way can go back to detection', () => {
    expect(markChoices(true, 'pack').map((c) => [c.label, c.mark])).toEqual([
      ['Not a pack', 'not_pack'],
      ['Detect pack automatically', null],
    ]);
    expect(markChoices(false, 'not_pack').map((c) => [c.label, c.mark])).toEqual([
      ['Mark as pack', 'pack'],
      ['Detect pack automatically', null],
    ]);
  });
});

describe('the pack edit form', () => {
  test('starts from your edits, and from the name shown when you gave none', () => {
    expect(packForm(pack('Car Kit'))).toEqual({ name: 'Car Kit', license: '', sourceUrl: '' });
    const edited = pack('My cars', { name: 'My cars', license: 'CC0', sourceUrl: 'https://k.nl' });
    expect(packForm(edited)).toEqual({
      name: 'My cars',
      license: 'CC0',
      sourceUrl: 'https://k.nl',
    });
  });

  test('saves trimmed fields, and empty ones as null to keep what was detected', () => {
    const form = { name: ' My cars ', license: ' ', sourceUrl: ' https://kenney.nl ' };
    expect(detailsEdit(form, pack('Car Kit'))).toEqual({
      kind: 'details',
      name: 'My cars',
      license: null,
      sourceUrl: 'https://kenney.nl',
    });
    expect(
      detailsEdit({ name: '', license: '', sourceUrl: '' }, pack('My cars', { name: 'x' }))
    ).toEqual({ kind: 'details', name: null, license: null, sourceUrl: null });
  });

  test('a name left as detected stays detected, but your own name stays yours', () => {
    const form = { name: 'Car Kit', license: 'MIT', sourceUrl: '' };
    expect(detailsEdit(form, pack('Car Kit'))).toMatchObject({ name: null, license: 'MIT' });
    expect(detailsEdit(form, pack('Car Kit', { name: 'Car Kit' }))).toMatchObject({
      name: 'Car Kit',
    });
  });

  test('a source URL must be a web address, or empty', () => {
    expect(sourceUrlProblem('')).toBeNull();
    expect(sourceUrlProblem('  https://kenney.nl/assets/car-kit ')).toBeNull();
    expect(sourceUrlProblem('HTTP://example.com')).toBeNull();
    for (const url of ['kenney.nl', 'javascript:alert(1)', 'file:///etc/passwd', 'https://']) {
      expect(sourceUrlProblem(url)).not.toBeNull();
    }
  });
});
