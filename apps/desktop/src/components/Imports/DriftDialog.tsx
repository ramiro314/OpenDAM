import { useEffect } from 'react';
import clsx from 'clsx';
import {
  choiceHint,
  choosableAll,
  followsItsFile,
  choiceLabel,
  differenceText,
  usePreparationStore,
  type DriftDraft,
} from '../../store/preparation';
import { useImportStore } from '../../store/imports';
import { IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

/**
 * The drift dialog of the one pack whose Rebuild waits for it, whichever Preparation asked:
 * mounted once, beside the app's other dialogs
 */
export function DriftDialogs() {
  const open = usePreparationStore((s) => Object.values(s.editors).find((e) => e.drift !== null));
  const imports = useImportStore((s) => s.imports);
  if (!open?.drift) return null;
  const pack = (imports[open.libraryId] ?? [])
    .flatMap((i) => i.packs)
    .find((p) => p.id === open.packId);
  return (
    <DriftDialog
      packId={open.packId}
      draft={open.drift}
      saving={open.saving}
      folder={pack?.target ?? ''}
    />
  );
}

interface Props {
  packId: string;
  draft: DriftDraft;
  saving: boolean;
  /** The pack folder, shown above the list and cut from each path */
  folder: string;
}

/**
 * Before a Rebuild: each file added, changed or missing in the pack folder since its last
 * build, with only the choices its origin can honour, the default chosen.
 */
export function DriftDialog({ packId, draft, saving, folder }: Props) {
  const { chooseDrift, chooseAllDrift, closeDrift, confirmDrift } = usePreparationStore.getState();

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      usePreparationStore.getState().closeDrift(packId);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [packId]);

  const prefix = folder ? `${folder}/` : '';
  const many = draft.differences.length !== 1;
  const restorable = choosableAll(draft, 'restore');
  return (
    <div className={dialog.overlay} onClick={() => closeDrift(packId)}>
      <div
        className={clsx(dialog.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="drift-dialog-title"
        data-drift-dialog={packId}
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <h3 id="drift-dialog-title" className={dialog.title}>
              Changed outside OpenDAM
            </h3>
          </div>
          <button
            className={dialog.closeBtn}
            onClick={() => closeDrift(packId)}
            disabled={saving}
            title="Close"
          >
            <IconX size={14} />
          </button>
        </div>
        <div className={dialog.content}>
          {folder && <span className={styles.path}>{folder}</span>}
          <span className={styles.hint}>
            {draft.differences.length === 0
              ? 'Nothing differs from the last build now.'
              : `${many ? `These ${draft.differences.length} files differ` : 'This file differs'} from the pack's last build. Choose what the rebuild does with ${many ? 'each' : 'it'}.`}
          </span>
          {restorable > 1 && (
            <div className={styles.prepRow}>
              <button
                className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
                disabled={saving}
                onClick={() => chooseAllDrift(packId, 'restore')}
                data-drift-restore-all
              >
                Restore all {restorable} missing files
              </button>
            </div>
          )}
          <ul className={styles.drift} data-drift-list>
            {draft.differences.map((d) => (
              <li key={d.path} className={styles.driftItem} data-drift-path={d.path}>
                <div className={styles.packLine}>
                  <code className={styles.target}>{d.path.slice(prefix.length) || d.path}</code>
                  <span className={styles.preset} data-drift-what>
                    {differenceText(d)}
                  </span>
                </div>
                <div className={styles.driftChoices} role="radiogroup" aria-label={d.path}>
                  {d.choices.map((choice) => (
                    <label key={choice} className={styles.check} title={choiceHint(choice, d)}>
                      <input
                        type="radio"
                        name={`drift-${d.path}`}
                        checked={draft.choices[d.path] === choice}
                        disabled={saving}
                        onChange={() => chooseDrift(packId, d.path, choice)}
                        data-drift-choice={choice}
                      />
                      {choiceLabel(choice, d)}
                    </label>
                  ))}
                </div>
                {followsItsFile(draft, d) && (
                  <span className={styles.hint} data-drift-follows>
                    Goes with its file, which this rebuild leaves out
                  </span>
                )}
                {d.reason && (
                  <span className={styles.hint} data-drift-reason>
                    {d.reason}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </div>
        <div className={dialog.actions}>
          <span className={clsx(styles.footerNote, styles.errorText)} data-drift-notice>
            {draft.notice}
          </span>
          <button
            className={clsx(dialog.btn, dialog.secondaryBtn)}
            onClick={() => closeDrift(packId)}
            disabled={saving}
          >
            Cancel
          </button>
          <button
            className={clsx(dialog.btn, dialog.primaryBtn)}
            disabled={saving}
            onClick={() => void confirmDrift(packId)}
            data-drift-rebuild
          >
            {saving ? 'Queuing…' : 'Rebuild'}
          </button>
        </div>
      </div>
    </div>
  );
}
