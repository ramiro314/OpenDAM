import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import { useAutotagStore } from '../../store';
import type { RunSummary } from '../../types';
import { modeLabel, runActions, runProgress, runUsage } from '../../utils/autotag';
import { IconTag, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Autotag.module.css';

const STATUS_LABELS: Record<RunSummary['status'], string> = {
  queued: 'Queued',
  running: 'Running',
  paused: 'Paused',
  awaiting_review: 'Needs review',
  done: 'Done',
  cancelled: 'Cancelled',
  undoing: 'Undoing',
  undo_partial: 'Partly undone',
  undone: 'Undone',
};

function statusClass(status: RunSummary['status']) {
  if (status === 'running' || status === 'queued') return styles.statusRunning;
  if (status === 'done') return styles.statusDone;
  if (status === 'paused' || status === 'awaiting_review') return styles.statusPaused;
  return undefined;
}

/** Runs a panel action; a refusal comes back as the panel's error line. */
type Act = (action: () => Promise<unknown>) => void;

function RunRow({ run, act }: { run: RunSummary; act: Act }) {
  const actions = runActions(run);
  const openReview = useAutotagStore((s) => s.openReview);
  const button = clsx(dialog.btn, dialog.secondaryBtn, styles.runBtn);
  const processed = run.done + run.failed;
  const percent = run.total > 0 ? (processed / run.total) * 100 : 0;
  const provider = run.model ? `${run.providerLabel} · ${run.model}` : run.providerLabel;
  const usage = runUsage(run);
  return (
    <li className={styles.run} data-run-id={run.id}>
      <div className={styles.runHeader}>
        <span className={styles.runLabel}>{run.label}</span>
        <span className={clsx(styles.status, statusClass(run.status))}>
          {STATUS_LABELS[run.status] ?? run.status}
        </span>
      </div>
      <span className={styles.runMeta}>
        {provider} · {modeLabel(run.mode)}
      </span>
      {run.pauseReason && <span className={styles.choiceReason}>{run.pauseReason}</span>}
      <div className={styles.bar}>
        <div className={styles.barFill} style={{ width: `${percent}%` }} />
      </div>
      <span className={styles.runProgressText}>
        {runProgress(run)}
        {usage && ` · ${usage}`}
      </span>
      {(actions.pause || actions.resume || actions.cancel || actions.review) && (
        <div className={styles.runActions}>
          {actions.review && (
            <button
              className={clsx(dialog.btn, dialog.primaryBtn, styles.runBtn)}
              onClick={() => openReview(run.id)}
              title="Accept, rename or reject the new tags this run proposed"
            >
              Review ({run.pendingProposals.toLocaleString()})
            </button>
          )}
          {actions.pause && (
            <button className={button} onClick={() => act(() => api.pauseAutotagRun(run.id))}>
              Pause
            </button>
          )}
          {actions.resume && (
            <button className={button} onClick={() => act(() => api.resumeAutotagRun(run.id))}>
              Resume
            </button>
          )}
          {actions.cancel && (
            <button
              className={button}
              onClick={() => act(() => api.cancelAutotagRun(run.id))}
              title="Skip the remaining assets. Tags already applied stay."
            >
              Cancel
            </button>
          )}
        </div>
      )}
    </li>
  );
}

/** Every tagging run, kept current by `autotag:updated` events. */
export function RunsPanel() {
  const runs = useAutotagStore((s) => s.runs);
  const setRunsPanelOpen = useAutotagStore((s) => s.setRunsPanelOpen);
  const [error, setError] = useState<string | null>(null);
  const onClose = () => setRunsPanelOpen(false);
  const paused = runs.filter((r) => r.status === 'paused').length;

  const act: Act = (action) => {
    setError(null);
    action().catch((e) => setError(String(e)));
  };
  const resumeAll = () =>
    act(async () => {
      const resumed = await api.resumeAllAutotagRuns();
      const left = paused - resumed;
      if (left > 0)
        setError(
          `${left} paused run${left === 1 ? '' : 's'} can't resume yet: the provider isn't ready`
        );
    });

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setRunsPanelOpen(false);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [setRunsPanelOpen]);

  return (
    <div className={dialog.overlay} onClick={onClose}>
      <div
        className={dialog.dialog}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="runs-title"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconTag size={16} />
            <h3 id="runs-title" className={dialog.title}>
              Tagging runs
            </h3>
          </div>
          <div className={styles.headerActions}>
            {paused > 0 && (
              <button
                id="runs-resume-all"
                className={clsx(dialog.btn, dialog.secondaryBtn, styles.runBtn)}
                onClick={resumeAll}
              >
                Resume all
              </button>
            )}
            <button className={dialog.closeBtn} onClick={onClose} title="Close">
              <IconX size={14} />
            </button>
          </div>
        </div>
        <div className={dialog.content}>
          {error && <p className={dialog.error}>{error}</p>}
          {runs.length === 0 ? (
            <p className={styles.empty}>No tagging runs yet.</p>
          ) : (
            <ul className={styles.runs}>
              {runs.map((run) => (
                <RunRow key={run.id} run={run} act={act} />
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}
