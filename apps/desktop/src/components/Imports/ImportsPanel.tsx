import { useEffect, useMemo } from 'react';
import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import { useLibraryStore, useUIStore } from '../../store';
import {
  FINISHING,
  cancelState,
  importRows,
  isLive,
  packStatus,
  progressPercent,
  progressText,
  skippedText,
  useImportStore,
  type PackRow,
} from '../../store/imports';
import { IconInbox, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

/**
 * Import pack: picks a `.unitypackage` or `.zip` original, then opens the dialog over it for the
 * library the sidebar has active, else the first one (the dialog can change it).
 */
export async function importPack() {
  const libraries = useLibraryStore.getState().libraries;
  if (libraries.length === 0) {
    useImportStore.setState({ error: 'Add a library first: a pack is imported into one' });
    return;
  }
  const picked = await open({
    multiple: false,
    directory: false,
    title: 'Import pack',
    filters: [{ name: 'Unity package or zip', extensions: ['unitypackage', 'zip'] }],
  });
  if (!picked || typeof picked !== 'string') return;
  const active = useUIStore.getState().activeLibraryId;
  const libraryId = libraries.some((l) => l.id === active) ? active! : libraries[0].id;
  await useImportStore.getState().openDialog(picked, libraryId);
}

function statusClass(row: PackRow) {
  if (row.run && isLive(row.run)) return styles.statusLive;
  if (row.run?.status === 'failed' || row.pack.status === 'failed') return styles.statusFailed;
  if (row.pack.status === 'built') return styles.statusDone;
  return undefined;
}

function Pack({ row }: { row: PackRow }) {
  const finishing = useImportStore((s) => s.finishing);
  const cancel = useImportStore((s) => s.cancel);
  const { run, progress } = row;
  const live = run !== null && isLive(run);
  const cancelShown = run ? cancelState(run, progress, finishing) : 'none';
  const percent = progressPercent(progress);
  return (
    <li className={styles.pack} data-pack-id={row.pack.id}>
      <div className={styles.packLine}>
        <span className={styles.target}>{row.pack.target}</span>
        <span className={styles.preset}>{row.pack.preset}</span>
        <span className={clsx(styles.status, statusClass(row))} data-pack-status>
          {packStatus(row)}
        </span>
        {cancelShown !== 'none' && run && (
          <button
            className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
            disabled={cancelShown === 'finishing'}
            onClick={() => cancel(run)}
            data-import-cancel
            title={
              cancelShown === 'finishing'
                ? 'The build holds the library and finishes: it commits, or rolls back on a failure'
                : 'Stop the build. Nothing in the library has changed yet.'
            }
          >
            {cancelShown === 'finishing' ? FINISHING : 'Cancel'}
          </button>
        )}
      </div>
      {live && run && (
        <>
          <div className={styles.bar}>
            <div
              className={clsx(styles.barFill, percent === null && styles.barBusy)}
              style={percent === null ? undefined : { width: `${percent}%` }}
            />
          </div>
          <span className={styles.progressText} data-import-progress>
            {progressText(run, progress)}
          </span>
        </>
      )}
      {run?.status === 'failed' && run.error && (
        <span className={styles.errorText}>{run.error}</span>
      )}
      {run?.status === 'interrupted' && (
        <span className={styles.warningText}>
          The run was interrupted when OpenDAM stopped. Its staged files were discarded.
        </span>
      )}
      {run && run.warnings.length > 0 && (
        <details className={styles.details}>
          <summary>
            {run.warnings.length === 1
              ? '1 entry of the original was skipped'
              : `${run.warnings.length} entries of the original were skipped`}
          </summary>
          <ul>
            {run.warnings.map((w, i) => (
              <li key={i}>{skippedText(w)}</li>
            ))}
          </ul>
        </details>
      )}
    </li>
  );
}

/** Every import of every open library, kept current by `import:updated` and `import:progress` */
export function ImportsPanel() {
  const imports = useImportStore((s) => s.imports);
  const runs = useImportStore((s) => s.runs);
  const progress = useImportStore((s) => s.progress);
  const error = useImportStore((s) => s.error);
  const setPanelOpen = useImportStore((s) => s.setPanelOpen);
  const libraries = useLibraryStore((s) => s.libraries);
  const rows = useMemo(
    () => importRows(Object.values(imports).flat(), runs, progress),
    [imports, runs, progress]
  );
  const libraryName = (id: string) => libraries.find((l) => l.id === id)?.name ?? id;
  const onClose = () => setPanelOpen(false);

  useEffect(() => {
    void useImportStore.getState().fetch(useLibraryStore.getState().libraries.map((l) => l.id));
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !useImportStore.getState().dialog) setPanelOpen(false);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [setPanelOpen]);

  return (
    <div className={dialog.overlay} onClick={onClose}>
      <div
        className={clsx(dialog.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="imports-title"
        id="imports-panel"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconInbox size={16} />
            <h3 id="imports-title" className={dialog.title}>
              Imports
            </h3>
          </div>
          <div className={styles.headerActions}>
            <button
              id="import-pack"
              className={clsx(dialog.btn, dialog.primaryBtn, styles.smallBtn)}
              onClick={() =>
                importPack().catch((e) => useImportStore.setState({ error: String(e) }))
              }
              title="Bring a .unitypackage or .zip into a library as one or more packs. The original is only read."
            >
              Import pack
            </button>
            <button className={dialog.closeBtn} onClick={onClose} title="Close">
              <IconX size={14} />
            </button>
          </div>
        </div>
        <div className={dialog.content}>
          {error && <p className={dialog.error}>{error}</p>}
          {rows.length === 0 ? (
            <p className={styles.empty}>
              No imports yet. Import pack reads a .unitypackage or .zip and builds its packs in a
              library, leaving the original untouched.
            </p>
          ) : (
            <ul className={styles.list}>
              {rows.map((row) => (
                <li key={row.key} className={styles.card} data-import-original={row.originalPath}>
                  <div className={styles.cardHeader}>
                    <span className={styles.name}>{row.name}</span>
                    {libraries.length > 1 && (
                      <span className={styles.library}>{libraryName(row.libraryId)}</span>
                    )}
                  </div>
                  <span className={styles.path}>{row.originalPath}</span>
                  <ul className={styles.packs}>
                    {row.packs.map((pack) => (
                      <Pack key={pack.pack.id} row={pack} />
                    ))}
                  </ul>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}
