import { create } from 'zustand';
import { api } from '../api';
import type { GroupMember } from '../types';

/**
 * The detail panel's group section (plan C19): the members of the focused card's format group,
 * and the member whose own details show instead of the group's. It never touches the grid's
 * selection or the asset store's pages, so moving between members shifts nothing in the grid.
 */
interface GroupDetailStore {
  /** The main item whose members are listed, or `null` */
  groupId: string | null;
  /** Its files, main item first, as `get_group_members` lists them */
  members: GroupMember[];
  /** The member whose details show, or `null` for the group's */
  memberId: string | null;
  /**
   * Lists the members of the group whose main item is `groupId`, or none for `null`. Another
   * group starts from its own details; the same group keeps the member shown while it is still a
   * member. When loads overlap, only the latest one's reply applies.
   */
  load: (groupId: string | null) => Promise<void>;
  /** Shows a listed member's details */
  showMember: (id: string) => void;
  /** Shows the group's details again */
  backToGroup: () => void;
}

let latestLoad = 0;

export const useGroupDetailStore = create<GroupDetailStore>()((set, get) => ({
  groupId: null,
  members: [],
  memberId: null,

  load: async (groupId) => {
    const load = ++latestLoad;
    if (groupId !== get().groupId) set({ groupId, members: [], memberId: null });
    if (!groupId) return;
    let members: GroupMember[];
    try {
      members = await api.getGroupMembers(groupId);
    } catch (err) {
      console.error('Failed to list group members:', err);
      return;
    }
    if (load !== latestLoad) return;
    set((s) => ({
      members,
      memberId: members.some((m) => m.id === s.memberId) ? s.memberId : null,
    }));
  },

  showMember: (id) => {
    if (get().members.some((m) => m.id === id)) set({ memberId: id });
  },

  backToGroup: () => set({ memberId: null }),
}));

/** The member whose details show, or `null` for the group's. */
export function shownMember(s: Pick<GroupDetailStore, 'members' | 'memberId'>): GroupMember | null {
  return s.members.find((m) => m.id === s.memberId) ?? null;
}
