import { useEffect } from 'react';
import clsx from 'clsx';
import { useAutotagStore } from '../../store';
import type { RunSummary } from '../../types';
import { modeLabel, runProgress } from '../../utils/autotag';
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

function RunRow({ run }: { run: RunSummary }) {
  const processed = run.done + run.failed;
  const percent = run.total > 0 ? (processed / run.total) * 100 : 0;
  const provider = run.model ? `${run.providerLabel} · ${run.model}` : run.providerLabel;
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
        {run.inputTokens + run.outputTokens > 0 &&
          ` · ${(run.inputTokens + run.outputTokens).toLocaleString()} tokens · $${run.costUsd.toFixed(2)}`}
      </span>
    </li>
  );
}

/** Every tagging run, kept current by `autotag:updated` events. */
export function RunsPanel() {
  const runs = useAutotagStore((s) => s.runs);
  const setRunsPanelOpen = useAutotagStore((s) => s.setRunsPanelOpen);
  const onClose = () => setRunsPanelOpen(false);

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
          <button className={dialog.closeBtn} onClick={onClose} title="Close">
            <IconX size={14} />
          </button>
        </div>
        <div className={dialog.content}>
          {runs.length === 0 ? (
            <p className={styles.empty}>No tagging runs yet.</p>
          ) : (
            <ul className={styles.runs}>
              {runs.map((run) => (
                <RunRow key={run.id} run={run} />
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}
