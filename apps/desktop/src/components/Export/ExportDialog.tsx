import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import type { Collection, ExportCheck } from '../../types';
import { checkSummary } from '../../utils/exportView';
import { IconExternal, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './ExportDialog.module.css';

interface ExportDialogProps {
  domain: Collection;
  onClose: () => void;
}

/**
 * A domain's Export view (plan game-export C14). "Check" reads the domain and its
 * subcollections, builds the manifest in memory and validates it, then lists every asset with
 * its kind, default name, pack and status, and every problem that would refuse the export.
 */
export function ExportDialog({ domain, onClose }: ExportDialogProps) {
  const [check, setCheck] = useState<ExportCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  const runCheck = async () => {
    setChecking(true);
    setError(null);
    try {
      setCheck(await api.checkExport(domain.id));
    } catch (e) {
      setError(String(e));
    } finally {
      setChecking(false);
    }
  };

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
              Check reads {domain.name} and its subcollections, and lists what the bundle would hold
              and what refuses the export. It writes nothing.
            </p>
          )}
          {error && <p className={dialog.error}>{error}</p>}
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
                      </li>
                    ))}
                  </ul>
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
                        <th>Role</th>
                        <th>Pack</th>
                        <th>Status</th>
                      </tr>
                    </thead>
                    <tbody>
                      {check.assets.map((a) => (
                        <tr key={`${a.libraryId}/${a.assetId}`} title={a.filePath}>
                          <td>{a.kind}</td>
                          <td className={styles.name}>{a.assetName ?? '-'}</td>
                          <td>{a.role ?? '-'}</td>
                          <td>{a.pack ?? 'none'}</td>
                          <td
                            className={clsx(
                              styles.status,
                              a.status === 'ready' ? styles.ready : styles.refused
                            )}
                          >
                            {a.status}
                          </td>
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
            className={clsx(dialog.btn, dialog.primaryBtn)}
            onClick={runCheck}
            disabled={checking}
          >
            {checking ? 'Checking…' : check ? 'Check again' : 'Check'}
          </button>
        </div>
      </div>
    </div>
  );
}
