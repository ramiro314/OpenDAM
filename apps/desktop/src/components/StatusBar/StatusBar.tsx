import { useEffect, useState } from 'react';
import { useAssetStore, useAutotagStore } from '../../store';
import { taggingSummary } from '../../utils/autotag';
import autotagStyles from '../Autotag/Autotag.module.css';
import { api } from '../../api';
import type { PreviewProgress } from '../../types';
import styles from './StatusBar.module.css';

const PROGRESS_POLL_MS = 3000;

/** Polls the preview queue; null once there's nothing left to generate. */
function usePreviewProgress(): PreviewProgress | null {
  const [progress, setProgress] = useState<PreviewProgress | null>(null);

  useEffect(() => {
    let cancelled = false;
    const poll = () =>
      api
        .getPreviewProgress()
        .then((p) => {
          if (cancelled) return;
          const busy = p.finished < p.total || p.modelsAwaitingRender > 0;
          setProgress(busy ? p : null);
        })
        .catch(() => {});
    poll();
    const interval = window.setInterval(poll, PROGRESS_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, []);

  return progress;
}

export function StatusBar() {
  const { total, loading, query } = useAssetStore();
  const previewProgress = usePreviewProgress();
  const tagging = taggingSummary(useAutotagStore((s) => s.runs));
  const setRunsPanelOpen = useAutotagStore((s) => s.setRunsPanelOpen);

  return (
    <footer className={styles.statusBar} id="status-bar">
      <div className={styles.left}>
        {loading ? (
          <span className={styles.item}>Indexing…</span>
        ) : (
          <span className={styles.item}>
            {total.toLocaleString()} asset{total !== 1 ? 's' : ''}
          </span>
        )}
        {query.text && (
          <span className={styles.item}>
            · Searching: <em>"{query.text}"</em>
          </span>
        )}
      </div>
      <div className={styles.right}>
        {previewProgress && (
          <span className={styles.item}>
            {previewProgress.finished < previewProgress.total &&
              `Generating previews: ${previewProgress.finished.toLocaleString()} / ${previewProgress.total.toLocaleString()}`}
            {previewProgress.finished < previewProgress.total &&
              previewProgress.modelsAwaitingRender > 0 &&
              ' · '}
            {previewProgress.modelsAwaitingRender > 0 &&
              `${previewProgress.modelsAwaitingRender.toLocaleString()} model${previewProgress.modelsAwaitingRender !== 1 ? 's' : ''} to render`}
          </span>
        )}
        {tagging && (
          <button
            id="status-tagging"
            className={autotagStyles.summaryBtn}
            onClick={() => setRunsPanelOpen(true)}
            title="Show tagging runs"
          >
            {tagging}
          </button>
        )}
        <span className={styles.item}>OpenDAM v0.1.0</span>
      </div>
    </footer>
  );
}
