import type { LicenseKind, PackDetail, PackEdit, PackMark } from '../types';

/** One pack mark item of a tree node's or pack card's context menu */
export interface MarkChoice {
  id: string;
  label: string;
  mark: PackMark | null;
}

/**
 * The pack mark items for a folder (plan C8 rule 4): "Not a pack" on a pack and "Mark as pack"
 * on any other folder, plus "Detect pack automatically" once you have marked it either way.
 */
export function markChoices(isPack: boolean, mark: PackMark | null | undefined): MarkChoice[] {
  const choices: MarkChoice[] = [
    isPack
      ? { id: 'folder-menu-not-pack', label: 'Not a pack', mark: 'not_pack' }
      : { id: 'folder-menu-mark-pack', label: 'Mark as pack', mark: 'pack' },
  ];
  if (mark) {
    choices.push({ id: 'folder-menu-detect-pack', label: 'Detect pack automatically', mark: null });
  }
  return choices;
}

/** The edit form's fields, as typed */
export interface PackForm {
  name: string;
  license: string;
  sourceUrl: string;
}

/** The form a pack's edit starts from: your name, else the one shown, and your other fields. */
export function packForm(pack: PackDetail): PackForm {
  return {
    name: pack.edits.name ?? pack.name,
    license: pack.edits.license ?? '',
    sourceUrl: pack.edits.sourceUrl ?? '',
  };
}

/**
 * The edit that saves `form` for `pack`. An empty field keeps what was detected, and so does the
 * name you left as detected, so a later license header still names the pack.
 */
export function detailsEdit(form: PackForm, pack: PackDetail): PackEdit {
  const text = (value: string) => value.trim() || null;
  const name = text(form.name);
  return {
    kind: 'details',
    name: pack.edits.name == null && name === pack.name ? null : name,
    license: text(form.license),
    sourceUrl: text(form.sourceUrl),
  };
}

/** Why `url` can't be a source URL, or `null` when it can (empty is fine: it clears it). */
export function sourceUrlProblem(url: string): string | null {
  const trimmed = url.trim();
  if (trimmed === '') return null;
  return /^https?:\/\/\S+$/i.test(trimmed) ? null : 'Use an address starting with https://';
}

/** The label of each license kind OpenDAM knows */
const LICENSE_LABEL: Record<Exclude<LicenseKind, 'unknown'>, string> = {
  CC0: 'CC0',
  'CC-BY': 'CC-BY',
  'CC-BY-SA': 'CC-BY-SA',
  MIT: 'MIT',
  'Apache-2.0': 'Apache-2.0',
  OFL: 'OFL',
};

/**
 * The license detected for `pack`, without yours: its known kind's label, else the license's
 * name, else "Unknown", or `null` when it has no license.
 */
export function detectedLicense(pack: PackDetail): string | null {
  const { license } = pack;
  if (!license) return null;
  if (license.kind !== 'unknown') return LICENSE_LABEL[license.kind] ?? license.kind;
  return license.name || 'Unknown';
}

/** The License row: your license, else the detected one, else "No license file". */
export function licenseLabel(pack: PackDetail): string {
  return pack.edits.license ?? detectedLicense(pack) ?? 'No license file';
}

/** The authors row's label: "Author", or "Authors" when there are several. */
export function authorsLabel(authors: string[]): string {
  return authors.length > 1 ? 'Authors' : 'Author';
}

/** The edit form's placeholders: the detected license and source URL, when there are. */
export function formPlaceholders(pack: PackDetail): { license: string; sourceUrl: string } {
  return {
    license: detectedLicense(pack) ?? 'CC0, CC-BY 4.0...',
    sourceUrl: pack.sourceUrl || 'https://',
  };
}
