import type { Collection, ExportCheck, ExportSettings } from '../types';

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

/** The line above the Export view's list: how many assets are ready and what refuses export. */
export function checkSummary(check: ExportCheck): string {
  const ready = check.assets.filter((a) => a.status === 'ready').length;
  const total = check.assets.length;
  const problems = check.problems.length;
  if (total === 0 && problems === 0) return 'The domain holds no assets yet';
  let assets = `${ready} of ${plural(total, 'asset')} ready`;
  if (total === 0) assets = 'No assets';
  else if (ready === total) assets = `All ${plural(total, 'asset')} ready`;
  if (problems === 0) return `${assets}. Nothing refuses the export`;
  return `${assets}. ${plural(problems, 'problem')} ${problems === 1 ? 'refuses' : 'refuse'} the export`;
}
