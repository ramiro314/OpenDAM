import type {
  AssetKind,
  ExportEditPatch,
  GameDetail,
  GameField,
  GameImage,
  GameMaterial,
  MaterialEdits,
  MaterialSurface,
  TextureLayoutEdit,
  TextureRole,
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

/**
 * The images a material's picker for `role` offers: the pack's images that give the role, by
 * path, and the one the role uses now (`current`, an asset id) even if it no longer gives it.
 * "None" is offered beside them.
 */
export function pickerOptions(
  images: GameImage[],
  role: TextureRole,
  current: string | null
): GameImage[] {
  return images.filter((i) => i.roles.includes(role) || i.assetId === current);
}

/**
 * Each image's path below the folder every one of `images` is in, by asset id, so a picker
 * reads `Textures/1K/T_Crate_N.png` rather than the pack's whole path.
 */
export function shortPaths(images: GameImage[]): Map<string, string> {
  const folders = images.map((i) => i.filePath.split('/').slice(0, -1));
  let shared = folders[0]?.length ?? 0;
  for (const folder of folders) {
    let n = 0;
    while (n < shared && n < folder.length && folder[n] === folders[0][n]) n++;
    shared = n;
  }
  return new Map(
    images.map((i) => [i.assetId, i.filePath.split('/').slice(shared).join('/')] as const)
  );
}

/** Whether a texture source's role is yours to pick: only when its layout passes one role through */
export function roleEditable(layout: TextureLayoutEdit): boolean {
  return layout === 'passThrough';
}

/** A change to one field of a material slot. A null name or surface, or an undefined texture, resets it */
export type SlotChange =
  | { field: 'name'; value: string | null }
  | { field: 'surface'; value: MaterialSurface | null }
  | { field: 'texture'; role: TextureRole; value: string | null | undefined };

/** The edits you hold on material `m`, as the patch that writes them */
export function slotEdits(m: GameMaterial): MaterialEdits {
  const edits: MaterialEdits = {};
  if (m.name.yours !== null) edits.name = m.name.yours;
  if (m.surface.yours !== null) edits.surface = m.surface.yours;
  // Your pick as stored, so another edit to the slot keeps one the export can't use
  const textures = Object.fromEntries(
    m.textures.filter((t) => t.yours).map((t) => [t.role, t.picked])
  );
  if (Object.keys(textures).length > 0) edits.textures = textures;
  return edits;
}

/**
 * The patch making `change` to material `m`, which keeps your other edits to the slot. A value
 * equal to the inferred one resets yours, so only real edits are stored (plan D32).
 */
export function slotPatch(m: GameMaterial, change: SlotChange): ExportEditPatch {
  const edits = slotEdits(m);
  const textures = { ...edits.textures };
  switch (change.field) {
    case 'name':
      if (change.value === null || change.value === m.name.inferred) delete edits.name;
      else edits.name = change.value;
      break;
    case 'surface':
      if (change.value === null || change.value === m.surface.inferred) delete edits.surface;
      else edits.surface = change.value;
      break;
    case 'texture': {
      const inferred = m.textures.find((t) => t.role === change.role)?.inferred?.assetId ?? null;
      if (change.value === undefined || change.value === inferred) delete textures[change.role];
      else textures[change.role] = change.value;
      if (Object.keys(textures).length > 0) edits.textures = textures;
      else delete edits.textures;
      break;
    }
  }
  return { materials: { [m.slot]: Object.keys(edits).length > 0 ? edits : null } };
}

/** The patch setting a texture source's `key` to `value`, or resetting it when it is the inferred one */
export function sourcePatch<K extends 'layout' | 'role' | 'normalConvention'>(
  key: K,
  field: GameField<NonNullable<ExportEditPatch[K]>>,
  value: NonNullable<ExportEditPatch[K]>
): ExportEditPatch {
  return { [key]: value === field.inferred ? null : value };
}

/** Why `draft` can't be a material's name, or null when it can */
export function materialNameError(draft: string): string | null {
  if (!draft) return 'A material’s name can’t be empty: Reset goes back to the inferred one';
  if (!isPascal(draft)) {
    return `${draft} isn’t PascalCase: a name starts with a capital letter, then only letters and digits`;
  }
  return null;
}
