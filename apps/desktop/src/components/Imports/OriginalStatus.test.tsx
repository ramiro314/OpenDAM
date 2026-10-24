import { afterEach, beforeAll, describe, expect, test } from 'bun:test';
import { act, create, type ReactTestInstance, type ReactTestRenderer } from 'react-test-renderer';
import { api } from '../../api';
import { CHANGED, MISSING, rowKey, useImportStore } from '../../store/imports';
import { usePreparationStore } from '../../store/preparation';
import type { ImportPackSummary, ImportSummary, OriginalState } from '../../types';
import { OriginalStatus } from './OriginalStatus';
import { Preparation } from './Preparation';

const real = { ...api };
const initialImports = useImportStore.getState();
const initialPreparation = usePreparationStore.getState();
const PATH = '/dl/Rocks.unitypackage';

beforeAll(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
});

/** Every renderer a test made, unmounted before the stores are reset under them */
let mounted: ReactTestRenderer[] = [];

afterEach(async () => {
  await act(async () => mounted.forEach((r) => r.unmount()));
  mounted = [];
  Object.assign(api, real);
  useImportStore.setState(initialImports, true);
  usePreparationStore.setState(initialPreparation, true);
});

const pack: ImportPackSummary = {
  id: 'p1',
  target: 'Pub/Rocks',
  preset: 'unity',
  status: 'built',
  builtAt: 1,
};

function imported(originalState: OriginalState, changedSha256: string | null = null) {
  const summary: ImportSummary = {
    id: 'i1',
    libraryId: 'lib',
    originalPath: PATH,
    kind: 'unitypackage',
    createdAt: 0,
    originalState,
    changedSha256,
    packs: [pack],
  };
  useImportStore.setState({ imports: { lib: [summary] } });
  return summary;
}

async function render(element: React.ReactElement): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(element);
  });
  mounted.push(renderer);
  return renderer;
}

const status = (state: OriginalState, changedSha256: string | null = null) => (
  <OriginalStatus
    libraryId="lib"
    originalPath={PATH}
    kind="unitypackage"
    state={state}
    changedSha256={changedSha256}
  />
);

const marked = (r: ReactTestRenderer, attr: string) =>
  r.root.findAll((n) => typeof n.type === 'string' && n.props[attr] !== undefined);

function text(node: ReactTestInstance): string {
  return node.children.map((c) => (typeof c === 'string' ? c : text(c))).join('');
}

describe('OriginalStatus', () => {
  test('an OK original shows nothing', async () => {
    const r = await render(status('ok'));
    expect(r.toJSON()).toBeNull();
  });

  test('a missing original names its state and reason, with Locate original', async () => {
    const r = await render(status('missing'));
    expect(text(marked(r, 'data-original-label')[0])).toBe('Original missing');
    expect(text(r.root.findByProps({ 'data-original-state': 'missing' }))).toContain(MISSING);
    expect(marked(r, 'data-original-locate')).toHaveLength(1);
    expect(marked(r, 'data-original-use')).toHaveLength(0);
  });

  test("a changed original offers Use this file, which names the changed file's sha256", async () => {
    const sent: unknown[] = [];
    api.locateImportOriginal = async (libraryId, originalPath, path, sha256 = null) => {
      sent.push([libraryId, originalPath, path, sha256]);
      return { kind: 'located' };
    };
    api.listImports = async () => [];
    api.listImportRuns = async () => [];
    const r = await render(status('changed', 'abc'));
    expect(text(marked(r, 'data-original-label')[0])).toBe('Original changed');
    expect(text(r.root.findByProps({ 'data-original-state': 'changed' }))).toContain(CHANGED);
    await act(async () => marked(r, 'data-original-use')[0].props.onClick());
    expect(sent).toEqual([['lib', PATH, PATH, 'abc']]);
  });

  test('a located file that differs waits for Use this file', async () => {
    useImportStore.setState({
      locating: {
        [rowKey('lib', PATH)]: { kind: 'differs', path: '/new/Other.unitypackage', sha256: 'd1' },
      },
    });
    const r = await render(status('missing'));
    expect(text(marked(r, 'data-original-differs')[0])).toContain(
      'Other.unitypackage isn’t the original'
    );
    expect(marked(r, 'data-original-use')).toHaveLength(1);
    expect(marked(r, 'data-original-locate')).toHaveLength(0);
  });
});

describe("Preparation's Rebuild", () => {
  const rebuildButton = (r: ReactTestRenderer) => marked(r, 'data-prep-rebuild')[0];

  test('is disabled with the reason while the original is missing or changed', async () => {
    api.getImportSteps = async () => ({
      recipeRev: 0,
      steps: [{ kind: 'extract_unitypackage', enabled: true, options: {} }],
      unavailable: {},
    });
    for (const [state, reason] of [
      ['missing', MISSING],
      ['changed', CHANGED],
    ] as const) {
      const original = imported(state, state === 'changed' ? 'abc' : null);
      const r = await render(
        <Preparation libraryId="lib" pack={pack} originalPath={original.originalPath} />
      );
      expect(rebuildButton(r).props.disabled).toBe(true);
      expect(rebuildButton(r).props.title).toBe(reason);
      expect(text(marked(r, 'data-prep-notice')[0])).toBe(reason);
      // The Preparation panel shows the original's state with it
      expect(marked(r, 'data-original-label')).toHaveLength(1);
      await act(async () => r.unmount());
      mounted = mounted.filter((m) => m !== r);
    }

    // Located: Rebuild is enabled again
    imported('ok');
    const r = await render(<Preparation libraryId="lib" pack={pack} originalPath={PATH} />);
    expect(rebuildButton(r).props.disabled).toBe(false);
    expect(marked(r, 'data-original-label')).toHaveLength(0);
  });
});
