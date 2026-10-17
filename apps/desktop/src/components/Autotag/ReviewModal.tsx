import { useEffect, useMemo, useReducer, useState } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import { api } from '../../api';
import { useAutotagStore, useTagStore } from '../../store';
import type { Tag } from '../../types';
import {
  applyProblem,
  existingTagNamed,
  reviewReducer,
  tagPath,
  toDecisions,
  type ProposalReview,
  type ReviewAction,
} from '../../utils/review';
import { IconTag, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Autotag.module.css';

interface TagOption {
  id: string;
  path: string;
}

function ProposalRow({
  review,
  tags,
  tagOptions,
  dispatch,
}: {
  review: ProposalReview;
  tags: Tag[];
  tagOptions: TagOption[];
  dispatch: (action: ReviewAction) => void;
}) {
  const { proposal, choice, name, parentId, mapTo, ticked } = review;
  const id = proposal.id;
  const mapped = mapTo !== null;
  const duplicate = !mapped ? existingTagNamed(name, tags) : null;
  const toggle = clsx(dialog.btn, dialog.secondaryBtn, styles.runBtn);

  return (
    <li
      className={clsx(
        styles.proposal,
        choice === 'accept' && styles.proposalAccepted,
        choice === 'reject' && styles.proposalRejected
      )}
      data-proposal-id={id}
    >
      <div className={styles.proposalHeader}>
        <input
          className={dialog.input}
          value={mapped ? (tagOptions.find((t) => t.id === mapTo)?.path ?? '') : name}
          disabled={mapped}
          onChange={(e) => dispatch({ type: 'rename', id, name: e.target.value })}
          aria-label={`Name for "${proposal.name}"`}
        />
        <button
          className={clsx(toggle, choice === 'accept' && styles.toggleOn)}
          aria-pressed={choice === 'accept'}
          onClick={() => dispatch({ type: 'accept', id })}
        >
          Accept
        </button>
        <button
          className={clsx(toggle, choice === 'reject' && styles.toggleOff)}
          aria-pressed={choice === 'reject'}
          onClick={() => dispatch({ type: 'reject', id })}
        >
          Reject
        </button>
      </div>
      <div className={styles.proposalFields}>
        <label className={styles.proposalField}>
          <span className={dialog.label}>Parent</span>
          <select
            className={dialog.input}
            value={parentId ?? ''}
            disabled={mapped}
            onChange={(e) => dispatch({ type: 'setParent', id, parentId: e.target.value || null })}
          >
            <option value="">No parent</option>
            {tagOptions.map((t) => (
              <option key={t.id} value={t.id}>
                {t.path}
              </option>
            ))}
          </select>
        </label>
        <label className={styles.proposalField}>
          <span className={dialog.label}>Map to existing tag</span>
          <select
            className={dialog.input}
            value={mapTo ?? ''}
            onChange={(e) => dispatch({ type: 'map', id, tagId: e.target.value || null })}
          >
            <option value="">Create a new tag</option>
            {tagOptions.map((t) => (
              <option key={t.id} value={t.id}>
                {t.path}
              </option>
            ))}
          </select>
        </label>
      </div>
      {duplicate && (
        <span className={styles.choiceNote}>
          A tag named "{duplicate.name}" exists: accepting applies it instead.
        </span>
      )}
      <div className={styles.thumbs}>
        {proposal.assets.map((asset) => (
          <label
            key={asset.assetId}
            className={clsx(styles.thumb, !ticked.includes(asset.assetId) && styles.thumbOff)}
            title={asset.fileName ?? 'Its library is offline'}
          >
            <input
              type="checkbox"
              checked={ticked.includes(asset.assetId)}
              onChange={() => dispatch({ type: 'toggleAsset', id, assetId: asset.assetId })}
            />
            {asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb') ? (
              <img src={convertFileSrc(asset.thumbnailPath)} alt="" loading="lazy" />
            ) : (
              <span className={styles.thumbPlaceholder}>
                {asset.fileName?.split('.').pop() ?? '?'}
              </span>
            )}
            <span className={styles.thumbName}>{asset.fileName ?? 'Offline'}</span>
          </label>
        ))}
      </div>
    </li>
  );
}

/**
 * The review of a run's new tag names: accept, rename, re-parent or map each one, untick
 * assets, or reject it, then Apply. Undecided proposals stay for later.
 */
export function ReviewModal({ runId }: { runId: string }) {
  const openReview = useAutotagStore((s) => s.openReview);
  const run = useAutotagStore((s) => s.runs.find((r) => r.id === runId));
  const tags = useTagStore((s) => s.tags);
  const tagMap = useTagStore((s) => s.tagMap);
  const fetchTags = useTagStore((s) => s.fetchTags);
  const [state, dispatch] = useReducer(reviewReducer, []);
  const [loaded, setLoaded] = useState(false);
  const [applying, setApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const onClose = () => openReview(null);

  useEffect(() => {
    api
      .getAutotagProposals(runId)
      .then((proposals) => {
        dispatch({ type: 'load', proposals });
        setLoaded(true);
      })
      .catch((e) => setError(String(e)));
  }, [runId]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') openReview(null);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [openReview]);

  const tagOptions = useMemo(
    () =>
      tags
        .map((t) => ({ id: t.id, path: tagPath(t, tagMap) }))
        .sort((a, b) => a.path.localeCompare(b.path)),
    [tags, tagMap]
  );

  const problem = applyProblem(state);
  const undecided = state.filter((r) => r.choice === 'undecided').length;

  const handleApply = async () => {
    setApplying(true);
    setError(null);
    try {
      await api.reviewAutotagProposals(runId, toDecisions(state));
      await fetchTags();
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setApplying(false);
    }
  };

  return (
    <div className={dialog.overlay} onClick={onClose}>
      <div
        className={clsx(dialog.dialog, styles.reviewDialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="review-title"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconTag size={16} />
            <h3 id="review-title" className={dialog.title}>
              Review new tags{run ? ` · ${run.label}` : ''}
            </h3>
          </div>
          <div className={styles.headerActions}>
            <button
              id="review-accept-all"
              className={clsx(dialog.btn, dialog.secondaryBtn, styles.runBtn)}
              onClick={() => dispatch({ type: 'acceptAll' })}
              disabled={state.length === 0}
            >
              Accept all
            </button>
            <button
              id="review-reject-all"
              className={clsx(dialog.btn, dialog.secondaryBtn, styles.runBtn)}
              onClick={() => dispatch({ type: 'rejectAll' })}
              disabled={state.length === 0}
            >
              Reject all
            </button>
            <button className={dialog.closeBtn} onClick={onClose} title="Close">
              <IconX size={14} />
            </button>
          </div>
        </div>
        <div className={dialog.content}>
          {error && <p className={dialog.error}>{error}</p>}
          {loaded && state.length === 0 && (
            <p className={styles.empty}>Nothing left to review in this run.</p>
          )}
          <ul className={styles.runs}>
            {state.map((review) => (
              <ProposalRow
                key={review.proposal.id}
                review={review}
                tags={tags}
                tagOptions={tagOptions}
                dispatch={dispatch}
              />
            ))}
          </ul>
        </div>
        <div className={dialog.actions}>
          <span className={clsx(styles.choiceNote, styles.actionsNote)}>
            {problem && state.length > 0
              ? problem
              : undecided > 0
                ? `${undecided} undecided stay${undecided === 1 ? 's' : ''} for later`
                : ''}
          </span>
          <button className={clsx(dialog.btn, dialog.secondaryBtn)} onClick={onClose}>
            Cancel
          </button>
          <button
            id="review-apply"
            className={clsx(dialog.btn, dialog.primaryBtn)}
            onClick={handleApply}
            disabled={problem !== null || applying}
          >
            {applying ? 'Applying…' : 'Apply'}
          </button>
        </div>
      </div>
    </div>
  );
}
