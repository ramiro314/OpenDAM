import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import {
  originalLabel,
  originalName,
  originalRefusal,
  rowKey,
  useImportStore,
} from '../../store/imports';
import type { ImportSummary, OriginalState } from '../../types';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

interface Props {
  libraryId: string;
  originalPath: string;
  kind: ImportSummary['kind'];
  state: OriginalState;
  /** While changed, the sha256 of the file at the original's path */
  changedSha256: string | null;
}

/**
 * An original's state, when it isn't OK: Original missing or Original changed, why Rebuild is
 * disabled, and Locate original. A located file that isn't the original, or the changed file at
 * its path, is used only through Use this file.
 */
export function OriginalStatus({ libraryId, originalPath, kind, state, changedSha256 }: Props) {
  const key = rowKey(libraryId, originalPath);
  const pending = useImportStore((s) => s.locating[key]);
  const { locate, dismissLocate } = useImportStore.getState();
  if (state === 'ok' && !pending) return null;
  const busy = pending?.kind === 'locating';

  const pick = async () => {
    const picked = await open({
      multiple: false,
      directory: false,
      title: `Locate ${originalName(originalPath)}`,
      filters: [
        kind === 'zip'
          ? { name: 'Zip', extensions: ['zip'] }
          : { name: 'Unity package', extensions: ['unitypackage'] },
      ],
    });
    if (picked && typeof picked === 'string') await locate(libraryId, originalPath, picked);
  };

  return (
    <div className={styles.original} data-original-state={state}>
      {state !== 'ok' && (
        <div className={styles.originalLine}>
          <span className={clsx(styles.status, styles.statusFailed)} data-original-label>
            {originalLabel(state)}
          </span>
          <span className={styles.warningText}>{originalRefusal(state)}</span>
        </div>
      )}
      {pending?.kind === 'differs' ? (
        <div className={styles.originalLine} data-original-differs>
          <span className={styles.warningText}>
            {pending.path === originalPath
              ? 'The file at the original’s path'
              : originalName(pending.path)}{' '}
            isn’t the original: its contents differ. Use it as the new original? Every pack’s next
            build reads it.
          </span>
          <button
            className={clsx(dialog.btn, dialog.primaryBtn, styles.smallBtn)}
            onClick={() => void locate(libraryId, originalPath, pending.path, pending.sha256)}
            title={pending.path}
            data-original-use
          >
            Use this file
          </button>
          <button
            className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
            onClick={() => dismissLocate(key)}
          >
            Cancel
          </button>
        </div>
      ) : (
        state !== 'ok' && (
          <div className={styles.originalLine}>
            <button
              className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
              disabled={busy}
              onClick={() => pick().catch((e) => useImportStore.setState({ error: String(e) }))}
              title="Pick the original where it is now. A file with the same contents takes its place at once."
              data-original-locate
            >
              {busy ? 'Checking the file…' : 'Locate original…'}
            </button>
            {state === 'changed' && changedSha256 && (
              <button
                className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
                disabled={busy}
                onClick={() => void locate(libraryId, originalPath, originalPath, changedSha256)}
                title="Make the changed file at the original's path the new original. Every pack's next build reads it."
                data-original-use
              >
                Use this file
              </button>
            )}
          </div>
        )
      )}
      {pending?.kind === 'error' && (
        <span className={styles.errorText} data-original-error>
          {pending.error}
        </span>
      )}
    </div>
  );
}
