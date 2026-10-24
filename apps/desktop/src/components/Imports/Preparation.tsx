import { useEffect } from 'react';
import clsx from 'clsx';
import { importOfPack, rebuildRefusal, skippedText, useImportStore } from '../../store/imports';
import {
  busyReason,
  draftProblem,
  fieldValue,
  isDirty,
  latestRun,
  optionFields,
  setOption,
  stepLabel,
  stepProblem,
  targetLocked,
  targetProblem,
  toggleProblem,
  toggleStep,
  usePreparationStore,
} from '../../store/preparation';
import type { ImportPackSummary, ImportStepRecord } from '../../types';
import { OriginalStatus } from './OriginalStatus';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

interface Props {
  libraryId: string;
  pack: ImportPackSummary;
  /** The original's absolute path, shown where the context doesn't already name it */
  originalPath?: string;
  /** The context already shows the latest run's error and warnings */
  runShown?: boolean;
}

/**
 * An imported pack's Preparation: its original, its target, its steps in order with their
 * toggles and options, the latest run's warnings, Rebuild and Remove import. The pack's details and the
 * Imports view open this same editor, which holds its edits by pack id.
 */
export function Preparation({ libraryId, pack, originalPath, runShown = false }: Props) {
  const editor = usePreparationStore((s) => s.editors[pack.id]);
  const runs = useImportStore((s) => s.runs);
  const original = useImportStore((s) => importOfPack(s.imports, libraryId, pack.id));
  const store = usePreparationStore.getState();
  useEffect(() => {
    void usePreparationStore.getState().open(libraryId, pack.id);
    return () => usePreparationStore.getState().close(pack.id);
  }, [libraryId, pack.id]);

  const busy = busyReason(runs, libraryId, pack.id);
  const run = runShown ? null : latestRun(runs, libraryId, pack.id);
  const draft = editor?.draft ?? null;
  const saving = editor?.saving ?? false;
  const locked = busy !== null || saving;
  const dirty = draft !== null && isDirty(draft);
  const problem = draft ? draftProblem(draft) : null;
  const notice = busy ?? editor?.error ?? (dirty ? problem : null);
  // A missing or changed original disables Rebuild, not the step edits
  const noRebuild = useImportStore((s) => rebuildRefusal(s.imports, libraryId, pack.id));

  const targetLock = targetLocked(pack);
  const typed = editor?.target ?? null;
  const targetIssue = typed ? targetProblem(typed, pack.target) : null;
  // The library checks a typed target once typing pauses
  useEffect(() => {
    if (!typed || typed.checked === typed.value) return;
    const timer = window.setTimeout(
      () => void usePreparationStore.getState().checkTarget(pack.id, pack.target),
      250
    );
    return () => window.clearTimeout(timer);
  }, [typed, pack.id, pack.target]);

  return (
    <div className={styles.prep} data-preparation={pack.id}>
      {originalPath && (
        <div className={styles.section}>
          <span className={styles.sectionLabel}>Original</span>
          <span className={styles.path} data-prep-original>
            {originalPath}
          </span>
          {original && (
            <OriginalStatus
              libraryId={libraryId}
              originalPath={original.originalPath}
              kind={original.kind}
              state={original.originalState}
              changedSha256={original.changedSha256}
            />
          )}
        </div>
      )}

      <div className={styles.section}>
        <span className={styles.sectionLabel}>Folder</span>
        {targetLock ? (
          <span className={styles.target} title={targetLock} data-prep-target>
            {pack.target}
          </span>
        ) : (
          <div className={styles.prepRow}>
            <input
              className={clsx(dialog.input, styles.prepInput)}
              value={typed?.value ?? pack.target}
              disabled={locked}
              onChange={(e) => store.editTarget(pack.id, e.target.value)}
              aria-label="Folder in the library"
              data-prep-target-input
            />
            {typed && (
              <button
                className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
                disabled={locked || targetIssue !== null}
                onClick={() => void store.moveTarget(pack.id, pack.target)}
                data-prep-move
              >
                Move
              </button>
            )}
          </div>
        )}
        {typed && targetIssue && <span className={styles.errorText}>{targetIssue}</span>}
      </div>

      <div className={styles.section}>
        <span className={styles.sectionLabel}>Steps</span>
        {!draft ? (
          <span className={styles.hint}>{editor?.loading ? 'Reading the steps…' : ''}</span>
        ) : (
          <ol className={styles.steps} data-prep-steps>
            {draft.steps.map((step, i) => (
              <Step
                key={i}
                step={step}
                index={i}
                packId={pack.id}
                locked={locked}
                toggleIssue={toggleProblem(step, draft.unavailable)}
                unavailable={draft.unavailable[step.kind] ?? null}
              />
            ))}
          </ol>
        )}
      </div>

      {draft && <KeptFiles target={pack.target} kept={draft.kept} />}

      {run && run.status === 'failed' && run.error && (
        <span className={styles.errorText} data-prep-run-error>
          {run.kind === 'remove' ? 'The Remove failed' : 'The last build failed'}: {run.error}
        </span>
      )}
      {run && run.warnings.length > 0 && (
        <details className={styles.details} data-prep-warnings>
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

      <div className={clsx(styles.footer, styles.prepFooter)}>
        <span
          className={clsx(styles.footerNote, busy === null && editor?.error && styles.errorText)}
          data-prep-notice
        >
          {notice ?? noRebuild ?? (dirty ? 'Saved steps change the pack at its next Rebuild' : '')}
        </span>
        {dirty && (
          <>
            <button
              className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
              disabled={saving}
              onClick={() => store.revert(pack.id)}
              data-prep-revert
            >
              Revert
            </button>
            <button
              className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
              disabled={locked || problem !== null}
              onClick={() => void store.save(pack.id)}
              data-prep-save
            >
              Save steps
            </button>
          </>
        )}
        <button
          className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
          disabled={locked}
          onClick={() => void store.askRemove(pack.id)}
          title="Delete the files the import put in this folder, kept files included, and drop its import records. Files added outside OpenDAM stay. What it deletes is listed first."
          data-prep-remove
        >
          Remove import
        </button>
        <button
          className={clsx(dialog.btn, dialog.primaryBtn, styles.smallBtn)}
          disabled={!draft || locked || problem !== null || noRebuild !== null}
          onClick={() => void store.rebuild(pack.id)}
          title={
            noRebuild ??
            'Build the pack again from its steps. Files changed outside OpenDAM are listed first, to keep or not. The pack stays as it is until the new build is done, and every file keeps its tags and metadata.'
          }
          data-prep-rebuild
        >
          {dirty ? 'Save and rebuild' : 'Rebuild'}
        </button>
      </div>
    </div>
  );
}

/** The pack's files kept from outside changes, which a rebuild carries but can't recreate */
export function KeptFiles({ target, kept }: { target: string; kept: string[] }) {
  if (kept.length === 0) return null;
  return (
    <div className={styles.section} data-prep-kept>
      <span className={styles.sectionLabel}>Not from the original</span>
      <span className={styles.hint}>
        Kept from outside changes: a rebuild carries them, but can't recreate them.
      </span>
      <ul className={styles.kept}>
        {kept.map((path) => (
          <li key={path} className={styles.path}>
            {path.startsWith(`${target}/`) ? path.slice(target.length + 1) : path}
          </li>
        ))}
      </ul>
    </div>
  );
}

function Step({
  step,
  index,
  packId,
  locked,
  toggleIssue,
  unavailable,
}: {
  step: ImportStepRecord;
  index: number;
  packId: string;
  locked: boolean;
  toggleIssue: string | null;
  unavailable: string | null;
}) {
  const edit = usePreparationStore((s) => s.edit);
  const fields = optionFields(step);
  const issue = stepProblem(step);
  return (
    <li
      className={clsx(styles.prepStep, !step.enabled && styles.stepDisabled)}
      data-prep-step={step.kind}
    >
      <label className={styles.check} title={toggleIssue ?? undefined}>
        <input
          type="checkbox"
          checked={step.enabled}
          disabled={locked || toggleIssue !== null}
          onChange={(e) => edit(packId, (d) => toggleStep(d, index, e.target.checked))}
          data-prep-toggle
        />
        {stepLabel(step)}
      </label>
      {unavailable && <span className={styles.preset}>can't run: {unavailable}</span>}
      {fields.length > 0 && (
        <div className={styles.prepOptions}>
          {fields.map((field) =>
            field.type === 'bool' ? (
              <label key={field.key} className={styles.check}>
                <input
                  type="checkbox"
                  checked={fieldValue(step, field) === true}
                  disabled={locked}
                  onChange={(e) =>
                    edit(packId, (d) => setOption(d, index, field.key, e.target.checked))
                  }
                  data-prep-option={field.key}
                />
                {field.label}
              </label>
            ) : (
              <label key={field.key} className={styles.prepField}>
                {field.label}
                <input
                  className={clsx(dialog.input, styles.inlineInput)}
                  value={fieldValue(step, field) as string}
                  inputMode={field.type === 'number' ? 'numeric' : undefined}
                  title={field.type === 'list' ? 'Separate names with commas' : undefined}
                  disabled={locked}
                  onChange={(e) =>
                    edit(packId, (d) => setOption(d, index, field.key, e.target.value))
                  }
                  data-prep-option={field.key}
                />
              </label>
            )
          )}
        </div>
      )}
      {issue && <span className={styles.errorText}>{issue}</span>}
    </li>
  );
}
