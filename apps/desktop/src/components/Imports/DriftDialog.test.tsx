import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { driftDraft } from '../../store/preparation';
import type { DriftDifference } from '../../types';
import { DriftDialog } from './DriftDialog';
import { KeptFiles } from './Preparation';

const difference = (
  path: string,
  change: DriftDifference['change'],
  kind: DriftDifference['kind'],
  choices: DriftDifference['choices'],
  reason: string | null = null
): DriftDifference => ({
  path: `Pub/Pack/${path}`,
  change,
  kind,
  size: change === 'missing' ? null : 4,
  sha256: change === 'missing' ? null : 'sha',
  choices,
  reason,
});

/** The markup's `<input>` tags */
const inputs = (html: string) => html.match(/<input[^>]*>/g) ?? [];

/** Each listed difference's radio choices, as `[choice, checked]`, by its path */
function choicesShown(html: string): Record<string, [string, boolean][]> {
  const shown: Record<string, [string, boolean][]> = {};
  for (const item of html.split('data-drift-path="').slice(1)) {
    const path = item.slice(0, item.indexOf('"'));
    const end = item.indexOf('</li>');
    shown[path] = inputs(item.slice(0, end)).map((input) => [
      /data-drift-choice="([^"]*)"/.exec(input)![1],
      input.includes('checked=""'),
    ]);
  }
  return shown;
}

/** The text of each `<li>` inside the element marked `marker` */
function listed(html: string, marker: string): string[] {
  const at = html.indexOf(marker);
  if (at < 0) return [];
  const list = html.slice(at, html.indexOf('</ul>', at));
  return [...list.matchAll(/<li[^>]*>([^<]*)<\/li>/g)].map((m) => m[1]);
}

describe('the drift dialog', () => {
  test('lists each difference with only the choices its origin offers, the default checked', () => {
    const glbReason =
      'No step drops one GLB: delete its model in the app, or turn the convert step off';
    const draft = driftDraft([
      difference('Notes.txt', 'added', 'outside', ['keep', 'discard']),
      difference('datapackage.json', 'changed', 'package', ['keep', 'recipe']),
      difference('Models/SM_Log.glb', 'missing', 'generated', ['restore'], glbReason),
      difference('Kept.png', 'missing', 'kept', ['forget'], 'A kept file can’t be recreated'),
    ]);
    const html = renderToStaticMarkup(
      <DriftDialog packId="p1" draft={draft} saving={false} folder="Pub/Pack" />
    );
    expect(choicesShown(html)).toEqual({
      'Pub/Pack/Notes.txt': [
        ['keep', true],
        ['discard', false],
      ],
      'Pub/Pack/datapackage.json': [
        ['keep', true],
        ['recipe', false],
      ],
      'Pub/Pack/Models/SM_Log.glb': [['restore', true]],
      'Pub/Pack/Kept.png': [['forget', true]],
    });
    expect(html).toContain('Models/SM_Log.glb');
    expect(html).not.toContain('>Pub/Pack/Models/SM_Log.glb<');
    expect(html).toContain(glbReason);
    expect(html).toContain('Changed, the pack&#x27;s metadata');
    expect(html).toContain('Keep yours');
    expect(html).toContain('These 4 files differ');
  });

  test('shows why the Rebuild failed, and locks its choices while it queues', () => {
    const draft = {
      ...driftDraft([difference('Notes.txt', 'added', 'outside', ['keep', 'discard'])]),
      notice: 'An import run is building this pack',
    };
    const html = renderToStaticMarkup(
      <DriftDialog packId="p1" draft={draft} saving={true} folder="Pub/Pack" />
    );
    expect(html).toContain('An import run is building this pack');
    expect(inputs(html).every((i) => i.includes('disabled=""'))).toBe(true);
    expect(html).toMatch(/data-drift-rebuild="?[^>]*>Queuing…</);
  });
});

describe("the Preparation panel's kept files", () => {
  test('lists them as not from the original, by their path in the pack', () => {
    const html = renderToStaticMarkup(
      <KeptFiles target="Pub/Pack" kept={['Pub/Pack/Notes.txt', 'Pub/Pack/Art/T_Rock.png']} />
    );
    expect(html).toContain('Not from the original');
    expect(listed(html, 'data-prep-kept')).toEqual(['Notes.txt', 'Art/T_Rock.png']);
  });

  test('shows no section for a pack without kept files', () => {
    expect(renderToStaticMarkup(<KeptFiles target="Pub/Pack" kept={[]} />)).toBe('');
  });
});
