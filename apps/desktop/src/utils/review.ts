import type { ProposalDecision, ReviewProposal, Tag } from '../types';

/** What the review will do with a proposal; `undecided` ones stay for a later review. */
export type ReviewChoice = 'undecided' | 'accept' | 'map' | 'reject';

export interface ProposalReview {
  proposal: ReviewProposal;
  choice: ReviewChoice;
  /** The name the new tag gets */
  name: string;
  parentId: string | null;
  /** The existing tag to apply instead, when mapped */
  mapTo: string | null;
  /** The ticked assets */
  ticked: string[];
}

export type ReviewAction =
  | { type: 'load'; proposals: ReviewProposal[] }
  | { type: 'accept'; id: string }
  | { type: 'reject'; id: string }
  | { type: 'rename'; id: string; name: string }
  | { type: 'setParent'; id: string; parentId: string | null }
  /** Maps onto an existing tag, or with null goes back to creating a new one */
  | { type: 'map'; id: string; tagId: string | null }
  | { type: 'toggleAsset'; id: string; assetId: string }
  | { type: 'acceptAll' }
  | { type: 'rejectAll' };

/** Every proposal undecided, named and placed as the model suggested, its assets ticked as stored. */
export function initialReview(proposals: ReviewProposal[]): ProposalReview[] {
  return proposals.map((proposal) => ({
    proposal,
    choice: 'undecided',
    name: proposal.name,
    parentId: proposal.parentId,
    mapTo: null,
    ticked: proposal.assets.filter((a) => a.included).map((a) => a.assetId),
  }));
}

export function reviewReducer(state: ProposalReview[], action: ReviewAction): ProposalReview[] {
  if (action.type === 'load') return initialReview(action.proposals);
  if (action.type === 'acceptAll' || action.type === 'rejectAll') {
    const choice = action.type === 'acceptAll' ? 'accept' : 'reject';
    // Accept all keeps a mapping you chose: it is a way of accepting.
    return state.map((r) => ({ ...r, choice: choice === 'accept' && r.mapTo ? 'map' : choice }));
  }
  return state.map((r) => {
    if (r.proposal.id !== action.id) return r;
    switch (action.type) {
      case 'accept':
        return { ...r, choice: r.mapTo ? 'map' : 'accept' };
      case 'reject':
        return { ...r, choice: 'reject' };
      // Editing a proposal means you want it
      case 'rename':
        return { ...r, name: action.name, choice: r.mapTo ? 'map' : 'accept' };
      case 'setParent':
        return { ...r, parentId: action.parentId, choice: r.mapTo ? 'map' : 'accept' };
      case 'map':
        return { ...r, mapTo: action.tagId, choice: action.tagId ? 'map' : 'accept' };
      case 'toggleAsset': {
        const ticked = r.ticked.includes(action.assetId)
          ? r.ticked.filter((a) => a !== action.assetId)
          : [...r.ticked, action.assetId];
        return { ...r, ticked };
      }
    }
  });
}

/** What Apply sends: one decision per decided proposal. */
export function toDecisions(state: ProposalReview[]): ProposalDecision[] {
  return state.flatMap((r): ProposalDecision[] => {
    const proposalId = r.proposal.id;
    // Keep the assets' order
    const assetIds = r.proposal.assets.map((a) => a.assetId).filter((a) => r.ticked.includes(a));
    switch (r.choice) {
      case 'undecided':
        return [];
      case 'reject':
        return [{ action: 'reject', proposalId }];
      case 'map':
        return [{ action: 'map', proposalId, tagId: r.mapTo!, assetIds }];
      case 'accept':
        return [
          { action: 'accept', proposalId, name: r.name.trim(), parentId: r.parentId, assetIds },
        ];
    }
  });
}

/** Why Apply can't go yet, or null when it can. */
export function applyProblem(state: ProposalReview[]): string | null {
  if (state.every((r) => r.choice === 'undecided')) return 'Accept or reject a proposal first';
  for (const r of state) {
    if (r.choice === 'accept' && !r.name.trim())
      return `Name the tag proposed as "${r.proposal.name}"`;
    if ((r.choice === 'accept' || r.choice === 'map') && r.ticked.length === 0)
      return `Tick an asset for "${r.name.trim() || r.proposal.name}", or reject it`;
  }
  return null;
}

/** The catalog tag a new name would duplicate (names are unique, ignoring case), so Apply maps onto it. */
export function existingTagNamed(name: string, tags: Tag[]): Tag | null {
  const folded = name.trim().toLowerCase();
  return tags.find((t) => t.name.toLowerCase() === folded) ?? null;
}

/** "Nature > Rock": the tag's name after its ancestors'. */
export function tagPath(tag: Tag, tagMap: Map<string, Tag>): string {
  const names = [tag.name];
  const seen = new Set([tag.id]);
  let parent = tag.parentId ? tagMap.get(tag.parentId) : undefined;
  while (parent && !seen.has(parent.id)) {
    seen.add(parent.id);
    names.unshift(parent.name);
    parent = parent.parentId ? tagMap.get(parent.parentId) : undefined;
  }
  return names.join(' > ');
}
