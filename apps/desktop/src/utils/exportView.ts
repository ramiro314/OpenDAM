import type {
  Collection,
  ExportCheck,
  ExportCheckAsset,
  ExportDiff,
  ExportDiffEntry,
  ExportMaterial,
  ExportProgress,
  ExportSettings,
} from '../types';

/**
 * Why "Export to game…" is disabled for `collection`, or null when it is a domain: a direct,
 * ordinary child of the root collection set in Settings (plan C3). `settings` is null while it
 * loads. The Check itself reports every other rule.
 */
export function exportMenuReason(
  collection: Pick<Collection, 'parentId' | 'isSmart'>,
  settings: Pick<ExportSettings, 'rootCollection'> | null,
  collections: Pick<Collection, 'id' | 'name'>[]
): string | null {
  if (!settings) return 'Loading the export settings…';
  if (!settings.rootCollection) return 'Set a root collection in Settings, under Game export';
  if (collection.isSmart) return 'A smart collection can’t be a domain';
  if (collection.parentId !== settings.rootCollection) {
    const root = collections.find((c) => c.id === settings.rootCollection);
    return `Only collections directly inside ${root ? root.name : 'the root collection'} export`;
  }
  return null;
}

const plural = (count: number, one: string, many = `${one}s`) =>
  `${count.toLocaleString()} ${count === 1 ? one : many}`;

/**
 * The line above the Export view's list: how many assets are ready (new, changed or unchanged)
 * and what refuses export.
 */
export function checkSummary(check: ExportCheck): string {
  const ready = check.assets.filter((a) => a.status !== 'problem').length;
  const total = check.assets.length;
  const problems = check.problems.length;
  if (total === 0 && problems === 0) return 'The domain holds no assets yet';
  let assets = `${ready} of ${plural(total, 'asset')} ready`;
  if (total === 0) assets = 'No assets';
  else if (ready === total) assets = `All ${plural(total, 'asset')} ready`;
  if (problems === 0) return `${assets}. Nothing refuses the export`;
  return `${assets}. ${plural(problems, 'problem')} ${problems === 1 ? 'refuses' : 'refuse'} the export`;
}

const VIA: Record<string, string> = {
  fbx: 'from the FBX',
  unity: 'from its Unity material',
  suffix: 'by file name',
  stem: 'by the model’s name',
  yours: 'yours',
};

/**
 * One line for a model's material in the Export view: its slot, name and surface, then each
 * role's texture and where it was found. `Stone → Stone, opaque: BaseColor Pillar (from the
 * FBX)`.
 */
export function materialLine(material: ExportMaterial): string {
  let head = `${material.slot} → ${material.name}, ${material.surface}`;
  if (material.cutoutSuggested) head += ' (cutout suggested)';
  if (material.added) head += ', added at export';
  const textures = Object.entries(material.textures).map(
    ([role, t]) => `${role} ${t.assetName} (${VIA[t.via] ?? t.via})`
  );
  return textures.length ? `${head}: ${textures.join(', ')}` : `${head}: no textures`;
}

/** An asset of the diff as the Export view lists it: `Rock (BaseColor)`, or a model's name. */
export function diffLabel(entry: ExportDiffEntry): string {
  return entry.role ? `${entry.assetName} (${entry.role})` : entry.assetName;
}

/** The line above the diff: what the export adds, changes and removes in the bundle. */
export function diffSummary(diff: ExportDiff): string {
  const { added, changed, removed, packs } = diff;
  if (diff.previousExportedAt === null) {
    return added.length === 0
      ? 'No bundle yet, and nothing to export'
      : `No bundle yet: the export adds ${plural(added.length, 'asset')}`;
  }
  const parts = [
    added.length > 0 && `${added.length} added`,
    changed.length > 0 && `${changed.length} changed`,
    removed.length > 0 && `${removed.length} removed`,
    packs.length > 0 && plural(packs.length, 'pack') + ' changed',
  ].filter(Boolean);
  if (parts.length === 0) return 'The bundle already holds exactly this';
  return `Against the previous bundle: ${parts.join(', ')}`;
}

/** Why Export wrote nothing: what changed since the Check, the first few by name. */
export function staleMessage(changes: string[]): string {
  const shown = changes.slice(0, 3).join('; ');
  const more = changes.length > 3 ? ` and ${changes.length - 3} more` : '';
  return `Nothing was exported: since the Check, this changed: ${shown}${more}. Check again`;
}

/**
 * The asset of `check` that an edit writing `ids` changed, or null. The Check no longer holds
 * what the domain exports then, so its staged bundle is stale (plan C14).
 */
export function editedAsset(
  check: Pick<ExportCheck, 'assets'>,
  ids: string[]
): ExportCheckAsset | null {
  const written = new Set(ids);
  return check.assets.find((a) => written.has(a.assetId)) ?? null;
}

/** What the Export view says once an edit made its Check stale. */
export function editedMessage(asset: Pick<ExportCheckAsset, 'filePath'>): string {
  return `You edited ${asset.filePath} since the Check. Check again to export it`;
}

/** What the Export view says while a Check's Blender jobs run (plan C14). */
export function progressMessage(
  progress: Pick<ExportProgress, 'done' | 'total' | 'asset'>
): string {
  const line = `Normalizing models in Blender: ${progress.done.toLocaleString()} of ${plural(progress.total, 'model')} done`;
  return progress.asset ? `${line}, last ${progress.asset.split('/').pop()}` : line;
}
