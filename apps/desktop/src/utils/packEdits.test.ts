import { describe, expect, test } from 'bun:test';
import type { LicenseKind, PackDetail, PackLicense, PackOverride } from '../types';
import {
  authorsLabel,
  detailsEdit,
  formPlaceholders,
  licenseLabel,
  markChoices,
  packForm,
  sourceUrlProblem,
} from './packEdits';

const pack = (
  name: string,
  edits: PackOverride = {},
  detected: Partial<PackDetail> = {}
): PackDetail => ({
  libraryId: 'lib',
  path: '3D assets/Car Kit',
  name,
  parentPath: '3D assets',
  itemCount: 1,
  hiddenCount: 0,
  gallery: [],
  kinds: {},
  sizeBytes: 0,
  authors: [],
  edits,
  ...detected,
});

const licensed = (license: PackLicense | null, edits: PackOverride = {}) =>
  pack('Kit', edits, { license });

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

describe('the pack license label', () => {
  test('is your license first', () => {
    const cc0 = { kind: 'CC0' as const, filePath: '/kit/License.txt' };
    expect(licenseLabel(licensed(cc0, { license: 'My terms' }))).toBe('My terms');
    expect(licenseLabel(licensed(null, { license: 'My terms' }))).toBe('My terms');
  });

  test('is a known kind by its label, even when the license is named', () => {
    const kinds: [LicenseKind, string][] = [
      ['CC0', 'CC0'],
      ['CC-BY', 'CC-BY'],
      ['CC-BY-SA', 'CC-BY-SA'],
      ['MIT', 'MIT'],
      ['Apache-2.0', 'Apache-2.0'],
      ['OFL', 'OFL'],
    ];
    for (const [kind, label] of kinds) {
      expect(licenseLabel(licensed({ kind }))).toBe(label);
      expect(licenseLabel(licensed({ kind, name: 'Some title' }))).toBe(label);
    }
  });

  test('is an unknown kind by its name, else "Unknown"', () => {
    const eula = { kind: 'unknown' as const, name: 'Standard Unity Asset Store EULA' };
    expect(licenseLabel(licensed(eula))).toBe('Standard Unity Asset Store EULA');
    expect(licenseLabel(licensed({ kind: 'unknown', name: null }))).toBe('Unknown');
    expect(licenseLabel(licensed({ kind: 'unknown', name: '' }))).toBe('Unknown');
  });

  test('says there is no license file when nothing names one', () => {
    expect(licenseLabel(licensed(null))).toBe('No license file');
  });
});

describe('the pack authors label', () => {
  test('is "Author" for one and "Authors" for several', () => {
    expect(authorsLabel(['Kenney'])).toBe('Author');
    expect(authorsLabel(['Ann', 'Bob'])).toBe('Authors');
  });
});

describe('the pack edit form placeholders', () => {
  test('show the detected source URL and license label', () => {
    const detected = pack(
      'Kit',
      { license: 'Mine', sourceUrl: 'https://mine.example.com' },
      {
        sourceUrl: 'https://store.example.com/kit',
        license: { kind: 'unknown', name: 'Standard Unity Asset Store EULA', url: 'https://u' },
      }
    );
    expect(formPlaceholders(detected)).toEqual({
      license: 'Standard Unity Asset Store EULA',
      sourceUrl: 'https://store.example.com/kit',
    });
    expect(formPlaceholders(licensed({ kind: 'CC0' })).license).toBe('CC0');
  });

  test('suggest what to type when nothing was detected', () => {
    expect(formPlaceholders(pack('Kit'))).toEqual({
      license: 'CC0, CC-BY 4.0...',
      sourceUrl: 'https://',
    });
  });
});
