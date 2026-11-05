import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ImportTree } from '../../types';
import { Tree } from './ImportDialog';

const tree = (patch: Partial<ImportTree> = {}): ImportTree => ({
  folders: [{ path: '', files: 3, total: 3 }],
  files: 3,
  glbs: 0,
  skipped: [],
  warnings: [],
  error: null,
  ...patch,
});

const render = (t: ImportTree) => renderToStaticMarkup(<Tree tree={t} converts={false} />);

describe('the tree preview', () => {
  test('shows each warning beside the tree it is about', () => {
    const html = render(tree({ warnings: ['hoist Art: no folder at Art'] }));
    expect(html).toContain('data-tree-warning');
    expect(html).toContain('hoist Art: no folder at Art');
    expect(html).toContain('(pack folder)');
  });

  test('shows no warning for a tree without any', () => {
    expect(render(tree())).not.toContain('data-tree-warning');
  });

  test("shows an estimate's note as a hint, not a warning", () => {
    const note = 'hoist Meshes: checked once the export runs';
    const html = render(tree({ estimate: true, glbs: 4, notes: [note] }));
    expect(html).toContain('data-tree-note');
    expect(html).toContain(note);
    expect(html).not.toContain('data-tree-warning');
  });
});
