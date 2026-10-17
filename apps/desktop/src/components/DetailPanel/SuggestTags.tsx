import { useCallback, useEffect, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import { useAutotagStore, useTagStore } from '../../store';
import type { AutotagSuggestion, ProposalDecision } from '../../types';
import { suggestionChips, suggestTagsAvailability, type SuggestionChip } from '../../utils/autotag';
import { tagPath } from '../../utils/review';
import { IconCheck, IconTag, IconX } from '../Icons';
import styles from './DetailPanel.module.css';

/**
 * "Suggest tags" for one asset (D17): an inline run with the last-used provider and mode. Its
 * existing-tag matches land in the tag list, and its new tag names show here as chips you
 * accept or reject, applied through the run's review.
 */
export function SuggestTags({ assetId }: { assetId: string }) {
  const providers = useAutotagStore((s) => s.providers);
  const lastUsed = useAutotagStore((s) => s.lastUsed);
  const tagMap = useTagStore((s) => s.tagMap);
  const fetchTags = useTagStore((s) => s.fetchTags);
  // Changes whenever an inline run moves on, so the chips follow it
  const inlineRuns = useAutotagStore((s) =>
    s.runs
      .filter((r) => r.inline)
      .map((r) => `${r.id}:${r.status}:${r.pendingProposals}`)
      .join(',')
  );
  const [suggestions, setSuggestions] = useState<AutotagSuggestion[]>([]);
  const [starting, setStarting] = useState(false);
  const [applying, setApplying] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reasonShown, setReasonShown] = useState(false);

  const load = useCallback(
    () => api.getAutotagSuggestions(assetId).then(setSuggestions, (e) => setError(String(e))),
    [assetId]
  );

  useEffect(() => {
    let current = true;
    api.getAutotagSuggestions(assetId).then(
      (s) => current && setSuggestions(s),
      (e) => current && setError(String(e))
    );
    return () => {
      current = false;
    };
  }, [assetId, inlineRuns]);

  const availability = suggestTagsAvailability(providers, lastUsed);
  const newest = suggestions[0]?.run;
  const inProgress = newest?.status === 'queued' || newest?.status === 'running';

  // A disabled button shows no tooltip in every webview, so a click shows the reason instead.
  const handleSuggest = async () => {
    if (!availability.enabled || !lastUsed) {
      setReasonShown((shown) => !shown);
      return;
    }
    setStarting(true);
    setError(null);
    try {
      await api.startAutotagRun(
        { kind: 'assets', assetIds: [assetId] },
        lastUsed.provider,
        lastUsed.mode,
        true
      );
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setStarting(false);
    }
  };

  // One decision per run holding the chip's name. Accepting in a second run maps onto the tag
  // the first one created.
  const decide = async (chip: SuggestionChip, accept: boolean) => {
    setApplying(chip.name);
    setError(null);
    try {
      for (const { runId, proposalId, assetIds } of chip.proposals) {
        const decision: ProposalDecision = accept
          ? {
              action: 'accept',
              proposalId,
              name: chip.name,
              parentId: chip.parentId,
              // The proposal's own asset, never whichever asset is shown now
              assetIds,
            }
          : { action: 'reject', proposalId };
        await api.reviewAutotagProposals(runId, [decision]);
      }
      if (accept) fetchTags().catch(console.error);
    } catch (e) {
      setError(String(e));
    } finally {
      setApplying(null);
      load();
    }
  };

  let status: string | null = null;
  if (starting || inProgress) status = 'Suggesting tags…';
  else if (newest?.status === 'paused') status = `Paused: ${newest.pauseReason ?? 'paused'}`;
  else if (newest && newest.failed > 0) {
    status = `Couldn't suggest tags: ${suggestions[0].error ?? 'the request failed'}`;
  }

  const chips = suggestionChips(suggestions);
  const busy = starting || inProgress;

  return (
    <div className={styles.suggest}>
      <div className={styles.suggestWrap}>
        <button
          id="detail-suggest-tags"
          className={clsx(
            styles.suggestBtn,
            (!availability.enabled || busy) && styles.suggestBtnDisabled
          )}
          onClick={handleSuggest}
          onBlur={() => setReasonShown(false)}
          aria-disabled={!availability.enabled || busy}
          disabled={busy}
          title={availability.tooltip}
        >
          <IconTag size={12} />
          <span>{busy ? 'Suggesting…' : 'Suggest tags'}</span>
        </button>
        {reasonShown && availability.reason && (
          <div className={styles.suggestReason} role="status">
            {availability.reason}
          </div>
        )}
      </div>
      {status && <p className={styles.suggestStatus}>{status}</p>}
      {error && <p className={clsx(styles.suggestStatus, styles.suggestError)}>{error}</p>}

      {chips.length > 0 && (
        <>
          <p className={styles.tagsGroupLabel}>Suggested new tags</p>
          <div className={styles.tagsGrid}>
            {chips.map((chip) => {
              const parent = chip.parentId ? tagMap.get(chip.parentId) : undefined;
              const where = parent ? ` under ${tagPath(parent, tagMap)}` : '';
              return (
                <span
                  key={chip.name}
                  className={styles.proposalChip}
                  title={`New tag "${chip.name}"${where}`}
                >
                  <span className={styles.tagPillName}>
                    {parent && <span className={styles.chipParent}>{parent.name} › </span>}
                    {chip.name}
                  </span>
                  <button
                    className={clsx(styles.chipBtn, styles.chipAccept)}
                    onClick={() => decide(chip, true)}
                    disabled={applying !== null}
                    title={`Create "${chip.name}"${where} and tag this asset`}
                    aria-label={`Accept ${chip.name}`}
                  >
                    <IconCheck size={11} />
                  </button>
                  <button
                    className={clsx(styles.chipBtn, styles.chipReject)}
                    onClick={() => decide(chip, false)}
                    disabled={applying !== null}
                    title={`Reject "${chip.name}"`}
                    aria-label={`Reject ${chip.name}`}
                  >
                    <IconX size={11} />
                  </button>
                </span>
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}
