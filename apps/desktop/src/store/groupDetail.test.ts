import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { useAssetStore, useUIStore } from '.';
import { shownMember, useGroupDetailStore } from './groupDetail';
import type { GroupMember } from '../types';

const realGetGroupMembers = api.getGroupMembers;

const member = (path: string) => {
  const slash = path.lastIndexOf('/');
  return {
    id: path,
    filePath: `/lib/${path}`,
    fileName: path.slice(slash + 1),
    extension: path.slice(path.lastIndexOf('.') + 1),
    folder: path.slice(0, slash),
  } as GroupMember;
};
const SEDAN = ['GLB format/sedan.glb', 'FBX format/sedan.fbx', 'OBJ format/sedan.obj'].map(member);
const VAN = ['GLB format/van.glb', 'OBJ format/van.obj'].map(member);

/** A `get_group_members` answering from `groups` (read on every call), recording calls. */
function serve(groups: () => Record<string, GroupMember[]>) {
  const calls: string[] = [];
  api.getGroupMembers = async (id) => {
    calls.push(id);
    return groups()[id] ?? [];
  };
  return calls;
}

const panel = () => useGroupDetailStore.getState();
const shownId = () => shownMember(panel())?.id ?? null;

afterEach(() => {
  api.getGroupMembers = realGetGroupMembers;
  useGroupDetailStore.setState({ groupId: null, members: [], memberId: null });
  useUIStore.getState().clearSelection();
});

describe('useGroupDetailStore', () => {
  test("lists a group's members, shows one, then goes back to the group", async () => {
    serve(() => ({ [SEDAN[0].id]: SEDAN }));
    await panel().load(SEDAN[0].id);
    expect(panel().members.map((m) => m.id)).toEqual(SEDAN.map((m) => m.id));
    expect(shownId()).toBeNull();

    panel().showMember(SEDAN[1].id);
    expect(shownMember(panel())).toEqual(SEDAN[1]);
    panel().showMember(SEDAN[2].id);
    expect(shownId()).toBe(SEDAN[2].id);
    panel().backToGroup();
    expect(shownId()).toBeNull();
  });

  test("moving between members leaves the grid's selection and pages as they were", async () => {
    serve(() => ({ [SEDAN[0].id]: SEDAN }));
    useUIStore.getState().selectAsset(SEDAN[0].id);
    const ui = useUIStore.getState();
    const { pages, pinned } = useAssetStore.getState();

    await panel().load(SEDAN[0].id);
    panel().showMember(SEDAN[1].id);
    panel().showMember(SEDAN[2].id);
    panel().backToGroup();

    const after = useUIStore.getState();
    expect(after.selectedAssetIds).toBe(ui.selectedAssetIds);
    expect(after.focusedAssetId).toBe(SEDAN[0].id);
    expect(useAssetStore.getState().pages).toBe(pages);
    expect(useAssetStore.getState().pinned).toBe(pinned);
  });

  test('another group starts from its own details, and no group lists nothing', async () => {
    const calls = serve(() => ({ [SEDAN[0].id]: SEDAN, [VAN[0].id]: VAN }));
    await panel().load(SEDAN[0].id);
    panel().showMember(SEDAN[1].id);

    await panel().load(VAN[0].id);
    expect(panel().members).toEqual(VAN);
    expect(shownId()).toBeNull();

    await panel().load(null);
    expect(panel()).toMatchObject({ groupId: null, members: [], memberId: null });
    expect(calls).toEqual([SEDAN[0].id, VAN[0].id]);
  });

  test('reloading a group keeps the member shown while it is still a member', async () => {
    let sedan = SEDAN;
    serve(() => ({ [SEDAN[0].id]: sedan }));
    await panel().load(SEDAN[0].id);
    panel().showMember(SEDAN[2].id);

    await panel().load(SEDAN[0].id);
    expect(shownId()).toBe(SEDAN[2].id);

    sedan = SEDAN.slice(0, 2);
    await panel().load(SEDAN[0].id);
    expect(panel().members).toEqual(sedan);
    expect(shownId()).toBeNull();
  });

  test('only the latest load applies when loads overlap', async () => {
    const replies: Record<string, (members: GroupMember[]) => void> = {};
    api.getGroupMembers = (id) => new Promise((resolve) => (replies[id] = resolve));
    const first = panel().load(SEDAN[0].id);
    const second = panel().load(VAN[0].id);
    replies[VAN[0].id](VAN);
    await second;
    replies[SEDAN[0].id](SEDAN);
    await first;
    expect(panel().groupId).toBe(VAN[0].id);
    expect(panel().members).toEqual(VAN);
  });

  test('a file that is not listed never shows', async () => {
    serve(() => ({ [SEDAN[0].id]: SEDAN }));
    await panel().load(SEDAN[0].id);
    panel().showMember(VAN[1].id);
    expect(shownId()).toBeNull();
  });
});
