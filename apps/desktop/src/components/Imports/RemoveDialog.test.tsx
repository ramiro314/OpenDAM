import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { DriftDifference, RemovePreview } from '../../types';
import { RemoveDialog } from './RemoveDialog';

const difference = (
  path: string,
  change: DriftDifference['change'],
  kind: DriftDifference['kind']
): DriftDifference => ({
  path: `Pub/Pack/${path}`,
  change,
  kind,
  size: change === 'missing' ? null : 4,
  sha256: change === 'missing' ? null : 'sha',
  choices: [],
  reason: null,
});

/** The text of each `<li>` inside the element marked `marker` */
function listed(html: string, marker: string): string[] {
  const at = html.indexOf(marker);
  if (at < 0) return [];
  const list = html.slice(at, html.indexOf('</ul>', at));
  return [...list.matchAll(/<li[^>]*>([^<]*)<\/li>/g)].map((m) => m[1]);
}

const render = (preview: RemovePreview, notice: string | null = null) =>
  renderToStaticMarkup(
    <RemoveDialog packId="p1" draft={{ preview, notice }} saving={false} folder="Pub/Pack" />
  );

describe('the Remove confirmation', () => {
  test('lists the files it deletes, names the kept ones, and the drift lost or left', () => {
    const html = render({
      files: [
        { path: 'Pub/Pack/Kept.png', origin: 'kept' },
        { path: 'Pub/Pack/Readme.txt', origin: 'extracted' },
        { path: 'Pub/Pack/Textures/T_Log_D.png', origin: 'extracted' },
        { path: 'Pub/Pack/datapackage.json', origin: 'generated' },
      ],
      drift: [
        difference('Notes.txt', 'added', 'outside'),
        difference('Readme.txt', 'changed', 'extracted'),
        difference('Textures/T_Log_D.png', 'missing', 'extracted'),
      ],
    });
    expect(html).toContain('Deletes the 4 files the import put in this folder');
    expect(listed(html, 'data-remove-files')).toEqual([
      'Kept.png',
      'Readme.txt',
      'Textures/T_Log_D.png',
      'datapackage.json',
    ]);
    expect(listed(html, 'data-remove-kept')).toEqual(['Kept.png']);
    expect(listed(html, 'data-remove-lost')).toEqual(['Readme.txt']);
    expect(listed(html, 'data-remove-staying')).toEqual(['Notes.txt']);
    expect(html).toContain('1 file is gone already.');
    expect(html).toContain('data-remove-confirm');
  });

  test('a pack without outside changes or kept files lists only its files', () => {
    const html = render({
      files: [{ path: 'Pub/Pack/Readme.txt', origin: 'extracted' }],
      drift: [],
    });
    expect(html).toContain('Deletes the 1 file the import put in this folder');
    for (const marker of ['data-remove-kept', 'data-remove-lost', 'data-remove-staying']) {
      expect(html).not.toContain(marker);
    }
    expect(html).not.toContain('data-remove-missing');
  });

  test('a pack never built has nothing to delete, and a refusal shows why', () => {
    const html = render({ files: [], drift: [] }, 'An import run is building this pack');
    expect(html).toContain('The pack has no files yet');
    expect(html).not.toContain('data-remove-files');
    expect(html).toContain('An import run is building this pack');
  });
});
