import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import type { Cover, PackCard } from '../types';
import { scopeKey, type PacksScope } from '../utils/packsView';
import { coverWithPreview, packKey, usePacksStore } from './packs';

const realGetPacks = api.getPacks;

const card = (path: string): PackCard => ({
  libraryId: 'kenney',
  path,
  name: path.slice(path.lastIndexOf('/') + 1),
  parentPath: path.slice(0, Math.max(0, path.lastIndexOf('/'))),
  itemCount: 1,
  hiddenCount: 0,
});

const root: PacksScope = { libraryIds: ['kenney'], underPath: '' };
const audio: PacksScope = { libraryIds: ['kenney'], underPath: 'Audio' };
const store = () => usePacksStore.getState();

afterEach(() => {
  api.getPacks = realGetPacks;
  usePacksStore.setState({ key: null, scope: null, packs: null, selected: null, mode: 'packs' });
});

const model: Cover = {
  assetId: 'm1',
  filePath: '/lib/kit/rock.glb',
  extension: 'glb',
  kind: '3d_model',
  memberIds: [],
};

describe('coverWithPreview', () => {
  const thumb = '/lib/.opendam/thumbs/m1_thumb.png';

  test("a cover takes its own asset's thumbnail, and no other's", () => {
    const done = { assetId: 'm1', thumbnailPath: thumb };
    expect(coverWithPreview(model, done).thumbnailPath).toBe(thumb);
    expect(coverWithPreview(model, { ...done, assetId: 'm2' })).toBe(model);
  });

  test('a cover that changes nothing stays the same object', () => {
    const cover = { ...model, thumbnailPath: thumb };
    expect(coverWithPreview(cover, { assetId: 'm1', thumbnailPath: thumb })).toBe(cover);
    expect(coverWithPreview(model, { assetId: 'm1', thumbnailPath: null })).toBe(model);
  });

  test("a job that clears its asset's thumbnail clears the cover's", () => {
    const cover = { ...model, thumbnailPath: thumb };
    expect(coverWithPreview(cover, { assetId: 'm1', thumbnailPath: null }).thumbnailPath).toBe(
      null
    );
  });

  test("a model's render switches to the render's thumbnail, and back when it is cleared", () => {
    const render = '/lib/kit/Previews/rock.png';
    const renderThumb = '/lib/.opendam/thumbs/r1_thumb.png';
    for (const field of ['bundledPreview', 'fallbackRender'] as const) {
      const shown = { ...model, [field]: render };
      const done = { assetId: 'r1', thumbnailPath: renderThumb, render: { filePath: render } };
      const thumbed = coverWithPreview(shown, done);
      expect(thumbed[field]).toBe(renderThumb);
      const cleared = {
        assetId: 'r1',
        thumbnailPath: null,
        render: { filePath: render, previousThumbnailPath: renderThumb },
      };
      expect(coverWithPreview(thumbed, cleared)[field]).toBe(render);
    }
  });

  test("another file's preview leaves a cover's render alone", () => {
    const other = {
      assetId: 'r2',
      thumbnailPath: '/lib/.opendam/thumbs/r2_thumb.png',
      render: {
        filePath: '/lib/kit/Previews/other.png',
        previousThumbnailPath: '/lib/.opendam/thumbs/r2_old.png',
      },
    };
    for (const field of ['bundledPreview', 'fallbackRender'] as const) {
      const shown = { ...model, [field]: '/lib/kit/Previews/rock.png' };
      expect(coverWithPreview(shown, other)).toBe(shown);
    }
  });
});

describe('usePacksStore', () => {
  test('a finished preview reaches the card it covers, and only that card', () => {
    const rock = { ...card('kit/rock'), cover: model };
    const other = { ...card('kit/other'), cover: { ...model, assetId: 'm2' } };
    usePacksStore.setState({ packs: [rock, other] });

    store().applyPreview({ assetId: 'm1', thumbnailPath: '/thumb.png' });
    expect(store().packs?.[0].cover?.thumbnailPath).toBe('/thumb.png');
    expect(store().packs?.[1]).toBe(other);

    // A preview that is no cover's leaves the list as it is, so no card renders again
    const packs = store().packs;
    store().applyPreview({ assetId: 'unrelated', thumbnailPath: '/other.png' });
    expect(store().packs).toBe(packs);

    // Nor does one that carries a render no card shows
    const rendered = {
      ...card('kit/rendered'),
      cover: { ...model, bundledPreview: '/render.png' },
    };
    usePacksStore.setState({ packs: [rendered] });
    const shown = store().packs;
    store().applyPreview({
      assetId: 'unrelated',
      thumbnailPath: '/other.png',
      render: { filePath: '/other-render.png', previousThumbnailPath: '/old.png' },
    });
    expect(store().packs).toBe(shown);
  });

  test("a member's preview reloads the packs, so the card shows what the grid's rule picks", async () => {
    const standIn = '/lib/.opendam/thumbs/obj_thumb.png';
    const blend = { ...model, assetId: 'blend', memberIds: ['obj'] };
    const before = { ...card('kit/rock'), cover: blend };
    const after = { ...before, cover: { ...blend, thumbnailPath: standIn } };
    const loads: string[] = [];
    api.getPacks = async (_ids, underPath) => {
      loads.push(underPath);
      return [loads.length > 1 ? after : before];
    };
    await store().load(root);
    expect(loads).toEqual(['']);

    // Not a member of any cover: nothing to ask
    store().applyPreview({ assetId: 'other', thumbnailPath: '/other.png' });
    expect(loads).toEqual(['']);
    // The cover isn't patched with it: a main's own thumbnail or an earlier member may win.
    store().applyPreview({ assetId: 'obj', thumbnailPath: '/not-picked.png' });
    expect(store().packs?.[0].cover?.thumbnailPath).toBeUndefined();
    expect(loads).toEqual(['', '']);
    await new Promise((resolve) => setTimeout(resolve));
    expect(store().packs?.[0].cover?.thumbnailPath).toBe(standIn);
  });

  test('a failed load leaves no packs, so the node shows Assets', async () => {
    api.getPacks = async () => {
      throw new Error('Library kenney is not open');
    };
    await store().load(root);
    expect(store().packs).toEqual([]);
    expect(store().key).toBe(scopeKey(root));
  });

  test('an earlier, slower load does not overwrite a later scope', async () => {
    let releaseRoot = () => {};
    api.getPacks = (_ids, underPath) =>
      underPath === ''
        ? new Promise((resolve) => (releaseRoot = () => resolve([card('3D assets/Car Kit')])))
        : Promise.resolve([card('Audio/Casino Audio')]);
    const slow = store().load(root);
    await store().load(audio);
    releaseRoot();
    await slow;
    expect(store().key).toBe(scopeKey(audio));
    expect(store().packs?.map((p) => p.path)).toEqual(['Audio/Casino Audio']);
  });

  test('a refetch keeps the selected card only while it is still listed', async () => {
    let paths = ['3D assets/Car Kit', '3D assets/Space Kit'];
    api.getPacks = async () => paths.map(card);
    await store().load(root);
    store().select(packKey(card('3D assets/Space Kit')));

    paths = ['3D assets/Car Kit', '3D assets/Space Kit', '3D assets/Tower Kit'];
    await store().refetch();
    expect(store().selected).toBe(packKey(card('3D assets/Space Kit')));

    paths = ['3D assets/Car Kit'];
    await store().refetch();
    expect(store().packs?.map((p) => p.path)).toEqual(['3D assets/Car Kit']);
    expect(store().selected).toBeNull();
  });

  test('loading the loaded scope again asks for nothing', async () => {
    let calls = 0;
    api.getPacks = async () => {
      calls++;
      return [card('3D assets/Car Kit')];
    };
    await store().load(root);
    await store().load({ ...root });
    expect(calls).toBe(1);
  });
});
