import type { AssetKind, ExportEditPatch, GameDetail, GameField } from '../types';

/** Whether `name` is a contract `pascalName`: a capital letter, then letters and digits */
export function isPascal(name: string): boolean {
  return /^[A-Z][A-Za-z0-9]*$/.test(name);
}

/** Whether the detail panel shows a Game tab for an asset of `kind`: models and images (C13) */
export function hasGameTab(kind: AssetKind): boolean {
  return kind === '3d_model' || kind === 'texture' || kind === 'image' || kind === 'hdri';
}

/** A value as the Game tab shows it: yours when you set one, else the inferred one, marked so */
export function shownValue<T>(field: GameField<T>): { value: T; mark: 'yours' | 'inferred' } {
  return field.yours === null
    ? { value: field.inferred, mark: 'inferred' }
    : { value: field.yours, mark: 'yours' };
}

/**
 * What saving `draft` as the asset's name writes, or why it can't be saved, or null when it
 * changes nothing. A name equal to the inferred one resets yours, so only real edits are stored
 * (plan D32).
 */
export function namePatch(
  field: GameField<string>,
  draft: string
): { patch: ExportEditPatch } | { error: string } | null {
  const name = draft.trim();
  if (name === shownValue(field).value) return null;
  if (name === field.inferred) return { patch: { assetName: null } };
  if (!name) return { error: 'A game name can’t be empty: Reset goes back to the inferred one' };
  if (!isPascal(name)) {
    return {
      error: `${name} isn’t PascalCase: a game name starts with a capital letter, then only letters and digits`,
    };
  }
  return { patch: { assetName: name } };
}

/** The line saying what the asset exports as */
export function exportsAs(detail: Pick<GameDetail, 'kind'>): string {
  switch (detail.kind) {
    case 'model':
      return 'Exports as a model';
    case 'animation':
      return 'Exports as an animation';
    case 'texture':
      return 'Exports as a texture. Its name is its texture set’s, shared by the maps beside it';
  }
}

/** The line saying which domains hold the asset */
export function domainsLine(detail: Pick<GameDetail, 'domains'>): string {
  const { domains } = detail;
  if (domains.length === 0)
    return 'In no domain yet: add it to a collection inside the root collection';
  return `In the domain${domains.length === 1 ? '' : 's'} ${domains.join(', ')}`;
}
