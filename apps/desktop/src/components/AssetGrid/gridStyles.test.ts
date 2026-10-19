import { describe, expect, test } from 'bun:test';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const dir = import.meta.dir;
const stylesheets = readdirSync(dir).filter((name) => name.endsWith('.css'));

/** The `backdrop-filter` declarations in `css`, with comments stripped. */
function backdropFilters(css: string): string[] {
  const code = css.replace(/\/\*[\s\S]*?\*\//g, '');
  return code.match(/(?:-webkit-)?backdrop-filter\s*:[^;}]*/g) ?? [];
}

describe('grid and list styles', () => {
  test('cover the card, the list row, the grid and the pack card', () => {
    expect(stylesheets).toEqual(
      expect.arrayContaining([
        'AssetCard.module.css',
        'AssetGrid.module.css',
        'AssetListRow.module.css',
        'PackCard.module.css',
      ])
    );
  });

  // Under GPU rendering (gpu-shm on NVIDIA), each backdrop-filter is a compositing layer, and once
  // the grid scrolls WebKitGTK 2.54's compositor re-renders them without end: the web process grew
  // about 500 MB/s until the OOM killer stopped it at 15-20 GB. The scroll bench's M1 check
  // measures this in the real webview.
  test.each(stylesheets)('%s declares no backdrop-filter', (name) => {
    expect(backdropFilters(readFileSync(join(dir, name), 'utf8'))).toEqual([]);
  });

  test('the check finds a backdrop-filter, prefixed or not, outside comments only', () => {
    expect(backdropFilters('.a { color: red; backdrop-filter: blur(4px); }')).toEqual([
      'backdrop-filter: blur(4px)',
    ]);
    expect(backdropFilters('.a{-webkit-backdrop-filter:blur(2px)}')).toEqual([
      '-webkit-backdrop-filter:blur(2px)',
    ]);
    expect(backdropFilters('/* no backdrop-filter: here */ .a { color: red; }')).toEqual([]);
  });
});
