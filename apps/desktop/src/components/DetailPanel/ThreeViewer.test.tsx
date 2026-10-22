import { afterAll, beforeAll, beforeEach, describe, expect, test } from 'bun:test';
import { Suspense } from 'react';
import { act, create, type ReactTestInstance, type ReactTestRenderer } from 'react-test-renderer';
import { FileLoader } from 'three';
import { useAssetStore } from '../../store';
import { Model, ThreeViewer, reloadModel } from './ThreeViewer';

const realWindow = globalThis.window;
// drei's GLTFLoader (three-stdlib) loads under bun through three's CommonJS build, a separate
// copy of `three`: stub the FileLoader of both.
const loaders: (typeof FileLoader)[] = [FileLoader, require('three').FileLoader];
const realLoads = loaders.map((l) => l.prototype.load);
const toUrl = (path: string) => `asset://localhost/${encodeURIComponent(path)}`;
/** The smallest glTF: one empty scene */
const EMPTY_GLTF = new TextEncoder().encode(
  JSON.stringify({ asset: { version: '2.0' }, scene: 0, scenes: [{ nodes: [] }] })
).buffer;
const a = '/lib/.opendam/thumbs/a_preview.glb';
const b = '/lib/.opendam/thumbs/b_preview.glb';

let fetched: string[];

beforeAll(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  (globalThis as { window: unknown }).window = {
    __TAURI_INTERNALS__: { convertFileSrc: (path: string) => toUrl(path) },
    // The Canvas measures its container; unmeasured, it never renders its scene here.
    ResizeObserver: class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
    addEventListener() {},
    removeEventListener() {},
  };
  // GLTFLoader reads every file through three's FileLoader: answer it with an empty scene.
  for (const loader of loaders) {
    loader.prototype.load = function (url, onLoad) {
      fetched.push(url);
      queueMicrotask(() => onLoad?.(EMPTY_GLTF.slice(0)));
      return undefined;
    };
  }
});

afterAll(() => {
  loaders.forEach((l, i) => (l.prototype.load = realLoads[i]));
  (globalThis as { window: unknown }).window = realWindow;
});

beforeEach(() => {
  fetched = [];
  useAssetStore.setState({ modelRevisions: {} });
});

async function until(predicate: () => boolean) {
  for (let i = 0; i < 100 && !predicate(); i++) {
    await act(() => new Promise((resolve) => setTimeout(resolve, 5)));
  }
  expect(predicate()).toBe(true);
}

/**
 * The host node React mounted for a test instance, which a remount replaces and an update
 * keeps (read from the fiber, as the renderer has no public handle on it)
 */
const hostNode = (n: ReactTestInstance) =>
  (n as unknown as { _fiber: { stateNode: object } })._fiber.stateNode;

describe('ThreeViewer', () => {
  test('a rerun remounts the viewer of that path, canvas included, and no other', async () => {
    let r!: ReactTestRenderer;
    await act(() => {
      r = create(
        <>
          <ThreeViewer modelPath={a} />
          <ThreeViewer modelPath={b} />
        </>
      );
    });
    const canvases = () => r.root.findAllByType('canvas').map(hostNode);
    const [oldA, oldB] = canvases();

    await act(() => reloadModel(a));

    const [newA, newB] = canvases();
    expect({ a: newA !== oldA, b: newB !== oldB }).toEqual({ a: true, b: false });
    act(() => r.unmount());
  });

  test("a model mounted after a rerun loads its GLB again, and another path's from cache", async () => {
    const mount = async (path: string) => {
      let r!: ReactTestRenderer;
      await act(() => {
        r = create(
          <Suspense fallback={null}>
            <Model url={path} />
          </Suspense>
        );
      });
      await until(() => r.root.findAll((n) => n.type === 'primitive').length === 1);
      const scene = r.root.find((n) => n.type === 'primitive').props.object;
      act(() => r.unmount());
      return scene;
    };
    const firstA = await mount(a);
    const firstB = await mount(b);
    expect(fetched).toEqual([toUrl(a), toUrl(b)]);

    reloadModel(a);

    expect(await mount(a)).not.toBe(firstA);
    expect(await mount(b)).toBe(firstB);
    expect(fetched).toEqual([toUrl(a), toUrl(b), toUrl(a)]);
  });
});
