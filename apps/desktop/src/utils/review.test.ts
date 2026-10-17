import { describe, expect, test } from 'bun:test';
import type { ReviewProposal, Tag } from '../types';
import {
  applyProblem,
  existingTagNamed,
  initialReview,
  reviewReducer,
  tagPath,
  toDecisions,
  type ReviewAction,
} from './review';

function proposal(id: string, name: string, assets: string[], parentId: string | null = null) {
  return {
    id,
    name,
    parentId,
    assets: assets.map((assetId) => ({
      assetId,
      fileName: `${assetId}.glb`,
      thumbnailPath: null,
      included: true,
    })),
  } satisfies ReviewProposal;
}

const PROPOSALS = [
  proposal('p-rock', 'Rock', ['a1', 'a2', 'b1'], 't-nature'),
  proposal('p-stones', 'Stones', ['a2']),
  proposal('p-junk', 'Junk', ['b1']),
];

function run(...actions: ReviewAction[]) {
  return actions.reduce(reviewReducer, reviewReducer([], { type: 'load', proposals: PROPOSALS }));
}

describe('the review-decision reducer', () => {
  test('starts undecided, with the suggested parent and every asset ticked', () => {
    const state = initialReview(PROPOSALS);

    expect(state[0]).toMatchObject({
      choice: 'undecided',
      name: 'Rock',
      parentId: 't-nature',
      mapTo: null,
      ticked: ['a1', 'a2', 'b1'],
    });
    expect(toDecisions(state)).toEqual([]);
    expect(applyProblem(state)).toBe('Accept or reject a proposal first');
  });

  test('accept, rename, re-parent, untick and reject become decisions', () => {
    const state = run(
      { type: 'rename', id: 'p-rock', name: '  Boulder ' },
      { type: 'setParent', id: 'p-rock', parentId: null },
      { type: 'toggleAsset', id: 'p-rock', assetId: 'a2' },
      { type: 'accept', id: 'p-stones' },
      { type: 'reject', id: 'p-junk' }
    );

    expect(toDecisions(state)).toEqual([
      {
        action: 'accept',
        proposalId: 'p-rock',
        name: 'Boulder',
        parentId: null,
        assetIds: ['a1', 'b1'],
      },
      {
        action: 'accept',
        proposalId: 'p-stones',
        name: 'Stones',
        parentId: null,
        assetIds: ['a2'],
      },
      { action: 'reject', proposalId: 'p-junk' },
    ]);
    expect(applyProblem(state)).toBeNull();
  });

  test('a ticked asset comes back in its original place', () => {
    const state = run(
      { type: 'toggleAsset', id: 'p-rock', assetId: 'a1' },
      { type: 'toggleAsset', id: 'p-rock', assetId: 'a1' },
      { type: 'accept', id: 'p-rock' }
    );

    expect(toDecisions(state)[0]).toMatchObject({ assetIds: ['a1', 'a2', 'b1'] });
  });

  test('mapping onto an existing tag, and back to a new tag', () => {
    const mapped = run({ type: 'map', id: 'p-stones', tagId: 't-stone' });

    expect(toDecisions(mapped)).toEqual([
      { action: 'map', proposalId: 'p-stones', tagId: 't-stone', assetIds: ['a2'] },
    ]);
    const unmapped = [{ type: 'map', id: 'p-stones', tagId: null } as const].reduce(
      reviewReducer,
      mapped
    );
    expect(unmapped[1]).toMatchObject({ choice: 'accept', mapTo: null });
  });

  test('accept all keeps mappings, reject all rejects everything', () => {
    const accepted = run(
      { type: 'map', id: 'p-stones', tagId: 't-stone' },
      { type: 'reject', id: 'p-junk' },
      { type: 'acceptAll' }
    );

    expect(accepted.map((r) => r.choice)).toEqual(['accept', 'map', 'accept']);
    const rejected = [{ type: 'rejectAll' } as const].reduce(reviewReducer, accepted);
    expect(toDecisions(rejected).every((d) => d.action === 'reject')).toBe(true);
  });

  test('a decision with no name or no ticked asset blocks Apply', () => {
    expect(applyProblem(run({ type: 'rename', id: 'p-rock', name: '  ' }))).toBe(
      'Name the tag proposed as "Rock"'
    );
    expect(
      applyProblem(
        run(
          { type: 'toggleAsset', id: 'p-stones', assetId: 'a2' },
          { type: 'accept', id: 'p-stones' }
        )
      )
    ).toBe('Tick an asset for "Stones", or reject it');
    expect(
      applyProblem(
        run({ type: 'toggleAsset', id: 'p-junk', assetId: 'b1' }, { type: 'reject', id: 'p-junk' })
      )
    ).toBeNull();
  });
});

const TAGS: Tag[] = [
  { id: 't-nature', name: 'Nature', description: '' },
  { id: 't-rock', name: 'Rock', parentId: 't-nature', description: '' },
  { id: 't-pebble', name: 'Pebble', parentId: 't-rock', description: '' },
];

describe('tag helpers', () => {
  test('existingTagNamed ignores case and spacing at the ends', () => {
    expect(existingTagNamed(' rock ', TAGS)?.id).toBe('t-rock');
    expect(existingTagNamed('Moss', TAGS)).toBeNull();
  });

  test('tagPath names the ancestors first', () => {
    const tagMap = new Map(TAGS.map((t) => [t.id, t]));
    expect(tagPath(TAGS[2], tagMap)).toBe('Nature > Rock > Pebble');
    expect(tagPath(TAGS[0], tagMap)).toBe('Nature');
  });
});
