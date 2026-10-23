import type {
  AssetKind,
  ExportEditPatch,
  GameDetail,
  GameField,
  GameModelDetail,
  GameRig,
} from '../types';

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

/**
 * What choosing `rig` writes, or null when it changes nothing. Choosing the inferred rig resets
 * yours, so only real edits are stored (plan D32).
 */
export function rigPatch(field: GameField<GameRig>, rig: GameRig): ExportEditPatch | null {
  if (rig === shownValue(field).value) return null;
  return { rig: rig === field.inferred ? null : rig };
}

/**
 * What a quarter turn writes: `by` 1 turns the model clockwise seen from above (taking its +Z to
 * +X), -1 back. Turned back to the origin's default, yours resets (plan D13, D32).
 */
export function turnPatch(field: GameField<number>, by: 1 | -1): ExportEditPatch {
  const turns = (shownValue(field).value + by + 4) % 4;
  return { facingQuarterTurns: turns === field.inferred ? null : turns };
}

/** The line saying which way a model faces: its origin's default, then your turns */
export function facingLine(model: Pick<GameModelDetail, 'facingQuarterTurns' | 'origin'>): string {
  const turns = shownValue(model.facingQuarterTurns).value;
  const base =
    model.origin === 'unreal'
      ? 'Its pack is from Unreal, so its +X is turned to face +Z'
      : model.origin
        ? `Its pack’s origin is ${model.origin}, so it faces as its file does`
        : 'It is in no pack yet';
  return turns === 0 ? base : `${base}, then turned ${turns * 90}° clockwise seen from above`;
}

/** A "Show as exported" answer: the GLB's path or why Blender can't make it, for `key` */
export type ViewResult = { key: string } & ({ path: string } | { error: string });

/** What the Game tab's viewer shows */
export type ExportedView =
  | { status: 'hidden' }
  | { status: 'loading' }
  | { status: 'shown'; path: string }
  | { status: 'failed'; error: string };

/**
 * The key of a model's "Show as exported" view: a new facing is another normalized model, so
 * the view asks for it again.
 */
export function viewKey(assetId: string, model: Pick<GameModelDetail, 'facingQuarterTurns'>) {
  return `${assetId}:${shownValue(model.facingQuarterTurns).value}`;
}

/**
 * What the viewer shows once you asked (`showing`), given the last answer: loading until the
 * answer is for the model's current `key`.
 */
export function exportedView(
  showing: boolean,
  result: ViewResult | null,
  key: string
): ExportedView {
  if (!showing) return { status: 'hidden' };
  if (result?.key !== key) return { status: 'loading' };
  return 'path' in result
    ? { status: 'shown', path: result.path }
    : { status: 'failed', error: result.error };
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
