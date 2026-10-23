import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import clsx from 'clsx';
import { api } from '../../api';
import { openGameTab, useExportEditStore } from '../../store';
import type {
  Collection,
  ExportAssetRef,
  ExportCheck,
  ExportDiffEntry,
  ExportProgress,
} from '../../types';
import {
  checkSummary,
  diffLabel,
  diffSummary,
  editedAsset,
  editedMessage,
  materialLine,
  progressMessage,
  staleMessage,
} from '../../utils/exportView';
import { IconExternal, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './ExportDialog.module.css';

interface ExportDialogProps {
  domain: Collection;
  onClose: () => void;
}

const statusClass = {
  ready: styles.ready,
  new: styles.pending,
  changed: styles.pending,
  problem: styles.refused,
};

/**
 * A domain's Export view (plan game-export C14). "Check" reads the domain and its
 * subcollections, builds the bundle in a staging folder beside the domain's and validates it,
 * then lists every asset with its kind, default name, pack and status, every problem that would
 * refuse the export, and the diff against the previous bundle. "Export" swaps the staged bundle
 * in, unless anything the Check read has changed since. Closing the view discards it.
 *
 * Each problem links to its assets' Game tab: the view shrinks to a bar while you edit there,
 * keeping its Check, and an edit to an asset the Check holds makes it ask to "Check again".
 */
export function ExportDialog({ domain, onClose }: ExportDialogProps) {
  const [check, setCheck] = useState<ExportCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  /** Shrunk to a bar while you edit an asset in the detail panel */
  const [minimized, setMinimized] = useState(false);
  /** An edit since the Check changed an asset it holds */
  const [edited, setEdited] = useState(false);
  /** How the running Check's Blender jobs are going */
  const [progress, setProgress] = useState<ExportProgress | null>(null);

  useEffect(() => {
    const unlisten = listen<ExportProgress>('export:progress', (e) => {
      if (e.payload.domainId === domain.id) setProgress(e.payload);
    });
    return () => {
      unlisten.then((stop) => stop()).catch(() => {});
    };
  }, [domain.id]);

  const lastEdit = useExportEditStore((s) => s.last);
  useEffect(() => {
    const asset = lastEdit && check ? editedAsset(check, lastEdit.ids) : null;
    if (!asset) return;
    setEdited(true);
    setNotice(editedMessage(asset));
    // Only a new edit counts: a Check run after it already holds it
  }, [lastEdit]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const id = domain.id;
    return () => {
      api.discardExport(id).catch((e) => console.warn('Could not discard the staged bundle', e));
    };
  }, [domain.id]);

  useEffect(() => {
    if (minimized) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose, minimized]);

  const runCheck = async () => {
    setChecking(true);
    setProgress(null);
    setError(null);
    setNotice(null);
    setEdited(false);
    try {
      setCheck(await api.checkExport(domain.id));
    } catch (e) {
      // A Check discards the earlier staged bundle before it runs, so the old report would show
      // an out-of-date diff with Export enabled for a bundle that is gone
      setCheck(null);
      setError(String(e));
    } finally {
      setChecking(false);
    }
  };

  const runExport = async () => {
    setExporting(true);
    setError(null);
    try {
      const outcome = await api.exportBundle(domain.id);
      setNotice(
        outcome.status === 'exported'
          ? `Exported ${outcome.assets.toLocaleString()} assets to ${outcome.bundlePath}`
          : staleMessage(outcome.changes)
      );
      // Either way the staged bundle is gone: a new Check stages the next one
      setCheck((c) => c && { ...c, staged: false });
    } catch (e) {
      setError(String(e));
      setCheck((c) => c && { ...c, staged: false });
    } finally {
      setExporting(false);
    }
  };

  const canExport =
    !!check?.staged && check.problems.length === 0 && !edited && !checking && !exporting;

  const showInGameTab = (asset: ExportAssetRef) => {
    setMinimized(true);
    openGameTab(asset.assetId).catch((e) => {
      setMinimized(false);
      setError(`Could not show ${asset.filePath}: ${String(e)}`);
    });
  };

  if (minimized) {
    return (
      <div id="export-minimized" className={styles.minimized} role="status">
        <IconExternal size={14} />
        <span className={styles.minimizedText}>
          Export {domain.name}:{' '}
          {edited ? 'edited since the Check' : check ? checkSummary(check) : 'not checked'}
        </span>
        <button
          id="export-restore"
          className={clsx(dialog.btn, dialog.primaryBtn)}
          onClick={() => setMinimized(false)}
        >
          {edited ? 'Back to Check again' : 'Back to Export'}
        </button>
        <button className={dialog.closeBtn} onClick={onClose} title="Close">
          <IconX size={14} />
        </button>
      </div>
    );
  }

  return (
    <div className={dialog.overlay} onClick={onClose}>
      <div
        className={clsx(dialog.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="export-title"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconExternal size={16} />
            <h3 id="export-title" className={dialog.title}>
              Export {domain.name} to game
            </h3>
          </div>
          <button className={dialog.closeBtn} onClick={onClose} title="Close">
            <IconX size={14} />
          </button>
        </div>

        <div className={dialog.content}>
          {!check && !error && (
            <p className={styles.hint}>
              Check reads {domain.name} and its subcollections, builds the bundle in a staging
              folder beside the previous one, and lists what it holds, what changed and what refuses
              the export. Export then swaps it in. The previous bundle stays until then.
            </p>
          )}
          {checking && progress && (
            <p id="export-progress" className={styles.summary} role="status">
              {progressMessage(progress)}
            </p>
          )}
          {error && <p className={dialog.error}>{error}</p>}
          {notice && (
            <p id="export-notice" className={styles.summary}>
              {notice}
            </p>
          )}
          {check && (
            <>
              {check.bundlePath && (
                <p className={styles.hint}>
                  Bundle: <span className={styles.path}>{check.bundlePath}</span>
                </p>
              )}
              <p id="export-summary" className={styles.summary}>
                {checkSummary(check)}
              </p>

              {check.problems.length > 0 && (
                <>
                  <h4 className={dialog.sectionTitle}>Problems</h4>
                  <ul id="export-problems" className={styles.problems}>
                    {check.problems.map((p, i) => (
                      <li key={i} className={styles.problem}>
                        {p.message}
                        {p.assets.map((a) => (
                          <button
                            key={`${a.libraryId}/${a.assetId}`}
                            className={styles.gameLink}
                            onClick={() => showInGameTab(a)}
                            title={`Show ${a.filePath} in the detail panel's Game tab`}
                          >
                            Game tab: {a.filePath.split('/').pop()}
                          </button>
                        ))}
                      </li>
                    ))}
                  </ul>
                </>
              )}

              {check.diff && (
                <>
                  <h4 className={dialog.sectionTitle}>Changes</h4>
                  <p id="export-diff" className={styles.hint}>
                    {diffSummary(check.diff)}
                    {check.diff.previousExportedAt &&
                      ` (exported ${new Date(check.diff.previousExportedAt).toLocaleString()})`}
                  </p>
                  {check.diff.previousExportedAt && (
                    <div className={styles.diff}>
                      <DiffList title="Added" entries={check.diff.added} />
                      <DiffList title="Changed" entries={check.diff.changed} />
                      <DiffList title="Removed" entries={check.diff.removed} />
                      {check.diff.packs.length > 0 && (
                        <div>
                          <span className={styles.diffTitle}>
                            Packs changed ({check.diff.packs.length})
                          </span>
                          <ul className={styles.diffList}>
                            {check.diff.packs.map((id) => (
                              <li key={id}>
                                pack <span className={styles.name}>{id}</span>
                              </li>
                            ))}
                          </ul>
                        </div>
                      )}
                    </div>
                  )}
                </>
              )}

              {check.assets.length > 0 && (
                <>
                  <h4 className={dialog.sectionTitle}>Assets</h4>
                  <table id="export-assets" className={styles.table}>
                    <thead>
                      <tr>
                        <th>Kind</th>
                        <th>Name</th>
                        <th>Layout</th>
                        <th>Pack</th>
                        <th>Status</th>
                      </tr>
                    </thead>
                    <tbody>
                      {check.assets.map((a) => (
                        <tr key={`${a.libraryId}/${a.assetId}`} title={a.filePath}>
                          <td>
                            {a.kind}
                            {!a.member && <span className={styles.joined}> (joined)</span>}
                          </td>
                          <td className={styles.name}>
                            {a.assetName ?? '-'}
                            {a.materials && a.materials.length > 0 && (
                              <ul className={styles.materials}>
                                {a.materials.map((m) => (
                                  <li key={m.slot}>{materialLine(m)}</li>
                                ))}
                              </ul>
                            )}
                          </td>
                          <td>
                            {a.layout ?? a.role ?? '-'}
                            {a.normalConvention === 'directx' && ', DirectX'}
                          </td>
                          <td>{a.pack ?? 'none'}</td>
                          <td className={clsx(styles.status, statusClass[a.status])}>{a.status}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </>
              )}
            </>
          )}
        </div>

        <div className={dialog.actions}>
          <button className={clsx(dialog.btn, dialog.secondaryBtn)} onClick={onClose}>
            Close
          </button>
          <button
            id="export-check"
            className={clsx(
              dialog.btn,
              check?.staged && !edited ? dialog.secondaryBtn : dialog.primaryBtn
            )}
            onClick={runCheck}
            disabled={checking}
          >
            {checking ? 'Checking…' : check ? 'Check again' : 'Check'}
          </button>
          <button
            id="export-export"
            className={clsx(dialog.btn, dialog.primaryBtn)}
            onClick={runExport}
            disabled={!canExport}
            title={canExport ? undefined : 'Run a Check with no problems first'}
          >
            {exporting ? 'Exporting…' : 'Export'}
          </button>
        </div>
      </div>
    </div>
  );
}

/** One list of the diff, by kind and name, left out when empty. */
function DiffList({ title, entries }: { title: string; entries: ExportDiffEntry[] }) {
  if (entries.length === 0) return null;
  return (
    <div>
      <span className={styles.diffTitle}>
        {title} ({entries.length})
      </span>
      <ul className={styles.diffList}>
        {entries.map((e) => (
          <li key={`${e.kind}/${e.assetName}/${e.role}`}>
            {e.kind} <span className={styles.name}>{diffLabel(e)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
