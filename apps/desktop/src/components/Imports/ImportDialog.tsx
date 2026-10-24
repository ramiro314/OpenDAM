import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { useLibraryStore } from '../../store';
import {
  candidateProblem,
  draftProblem,
  editPublisher,
  editTarget,
  originalName,
  patchCandidate,
  pickVariant,
  setHoist,
  skippedText,
  treeKey,
  useImportStore,
  type DraftCandidate,
  type ImportDraft,
} from '../../store/imports';
import type { ImportOffer, ImportTree, ImportVariant } from '../../types';
import { IconInbox, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

/** How long an edit settles before the library is asked about it */
const SETTLE_MS = 300;

/** Folders a tree preview lists before it says how many more there are */
const TREE_ROWS = 200;

const OFFER_LABELS: Record<ImportOffer['layout'], string> = {
  unity: 'Unity package',
  fab: 'Fab Unity download',
  kenney: 'Kenney bundle',
  zip: 'Any other zip',
};

const PIPELINES: Record<NonNullable<ImportVariant['pipeline']>, string> = {
  urp: 'URP',
  'built-in': 'Built-in',
  hdrp: 'HDRP',
};

function plural(n: number, one: string, many = `${one}s`) {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}

function Tree({ tree, converts }: { tree: ImportTree; converts: boolean }) {
  if (tree.error) return <span className={styles.errorText}>{tree.error}</span>;
  const shown = tree.folders.slice(0, TREE_ROWS);
  return (
    <>
      <span className={styles.hint} data-tree-summary>
        {plural(tree.files, 'file')} in {plural(tree.folders.length, 'folder')}
        {converts && tree.glbs > 0 && ` · ${plural(tree.glbs, 'model')} to convert to GLB`}
      </span>
      <ul className={styles.tree}>
        {shown.map((f) => {
          const depth = f.path ? f.path.split('/').length : 0;
          const name = f.path ? f.path.split('/').pop() : '(pack folder)';
          return (
            <li key={f.path} className={styles.treeRow} style={{ paddingLeft: depth * 12 }}>
              <span>{name}/</span>
              <span className={styles.treeCount}>
                {f.files === f.total ? f.total : `${f.files} · ${f.total} in all`}
              </span>
            </li>
          );
        })}
        {tree.folders.length > shown.length && (
          <li className={styles.treeCount}>
            and {plural(tree.folders.length - shown.length, 'more folder')}
          </li>
        )}
      </ul>
      {tree.skipped.length > 0 && (
        <details className={styles.details}>
          <summary>{plural(tree.skipped.length, 'entry', 'entries')} would be skipped</summary>
          <ul>
            {tree.skipped.map((w, i) => (
              <li key={i}>{skippedText(w)}</li>
            ))}
          </ul>
        </details>
      )}
    </>
  );
}

function Candidate({
  draft,
  index,
  expandedByDefault,
}: {
  draft: ImportDraft;
  index: number;
  expandedByDefault: boolean;
}) {
  const c: DraftCandidate = draft.candidates[index];
  const edit = useImportStore((s) => s.edit);
  const checkTarget = useImportStore((s) => s.checkTarget);
  const refreshTree = useImportStore((s) => s.refreshTree);
  const [expanded, setExpanded] = useState(expandedByDefault);
  const problem = candidateProblem(draft, index);
  // Why the conversion can't run here (no accepted Blender), so the import records it off
  const convertOff = draft.unavailable.unity_glb ?? null;
  const key = treeKey(c);
  const set = (patch: Partial<DraftCandidate>) => edit((d) => patchCandidate(d, index, patch));

  // A changed target is checked once it settles, as a changed source or step is previewed
  useEffect(() => {
    if (c.checkedTarget === c.target) return;
    const timer = window.setTimeout(() => void checkTarget(index), SETTLE_MS);
    return () => window.clearTimeout(timer);
  }, [c.target, c.checkedTarget, index, checkTarget]);
  useEffect(() => {
    if (c.treeKey === key) return;
    const timer = window.setTimeout(() => void refreshTree(index), SETTLE_MS);
    return () => window.clearTimeout(timer);
  }, [key, c.treeKey, index, refreshTree]);

  const extract = c.variants.length
    ? 'the ticked variant'
    : c.subpath
      ? c.subpath
      : draft.kind === 'unitypackage'
        ? 'the whole package'
        : 'the whole zip';

  return (
    <li className={styles.card} data-candidate={c.name}>
      <div className={styles.candidateHeader}>
        <input
          type="checkbox"
          checked={c.ticked}
          disabled={c.imported}
          onChange={(e) => set({ ticked: e.target.checked })}
          aria-label={`Import ${c.name}`}
        />
        <span className={styles.candidateName}>{c.name}</span>
        {c.imported ? (
          <span className={styles.imported}>Queued</span>
        ) : (
          <button className={styles.expand} onClick={() => setExpanded(!expanded)}>
            {expanded ? 'Hide steps' : 'Steps and tree'}
          </button>
        )}
      </div>
      <label className={styles.field}>
        Folder in the library
        <input
          className={dialog.input}
          value={c.target}
          disabled={c.imported}
          onChange={(e) => edit((d) => editTarget(d, index, e.target.value))}
          onBlur={() => void checkTarget(index)}
          data-candidate-target
        />
      </label>
      {(c.error || (c.ticked && problem)) && (
        <span className={styles.warningText} data-candidate-problem>
          {c.error ?? problem}
        </span>
      )}
      {expanded && !c.imported && (
        <>
          {c.variants.length > 0 && (
            <div className={styles.section}>
              <span className={styles.sectionLabel}>Variant</span>
              {c.variants.map((v, j) => (
                <label key={`${v.variant}/${v.subpath ?? ''}`} className={styles.variant}>
                  <input
                    type="radio"
                    name={`variant-${index}`}
                    checked={v.ticked}
                    onChange={() => edit((d) => pickVariant(d, index, j))}
                  />
                  <code>
                    {v.variant}
                    {v.subpath ? ` / ${v.subpath}` : ''}
                  </code>
                  {v.pipeline && <span className={styles.preset}>{PIPELINES[v.pipeline]}</span>}
                </label>
              ))}
            </div>
          )}
          <div className={styles.section}>
            <span className={styles.sectionLabel}>Steps</span>
            <ol className={styles.steps}>
              <li className={styles.step}>
                Extract <code>{extract}</code>
              </li>
              <li className={styles.step}>
                <label className={styles.check}>
                  <input
                    type="checkbox"
                    checked={c.flatten}
                    onChange={(e) => set({ flatten: e.target.checked })}
                  />
                  Flatten wrapper folders
                </label>
              </li>
              <li className={styles.step}>
                <label className={styles.check}>
                  <input
                    type="checkbox"
                    checked={c.hoistOn}
                    onChange={(e) => edit((d) => setHoist(d, index, e.target.checked))}
                  />
                  Move up the contents of
                </label>
                <input
                  className={clsx(dialog.input, styles.inlineInput)}
                  value={c.hoist}
                  placeholder="folder"
                  onChange={(e) => set({ hoist: e.target.value })}
                  aria-label="Folder to move up"
                />
              </li>
              {c.converts && (
                <li
                  className={clsx(styles.step, convertOff && styles.stepDisabled)}
                  data-convert-step
                >
                  Convert models to GLB
                  <span className={styles.preset}>
                    {convertOff ? `off: ${convertOff}` : 'beside each model, in Blender'}
                  </span>
                </li>
              )}
              <li className={styles.step}>
                Write <code>datapackage.json</code> ({c.preset} preset)
              </li>
            </ol>
            <div className={styles.fieldRow}>
              <label className={styles.field}>
                Title
                <input
                  className={dialog.input}
                  value={c.title}
                  placeholder={c.name}
                  onChange={(e) => set({ title: e.target.value })}
                />
              </label>
              <label className={styles.field}>
                Version
                <input
                  className={dialog.input}
                  value={c.version}
                  onChange={(e) => set({ version: e.target.value })}
                />
              </label>
              <label className={styles.field}>
                License
                <input
                  className={dialog.input}
                  value={c.license}
                  placeholder={c.preset === 'kenney' ? 'CC0-1.0' : 'From the preset'}
                  onChange={(e) => set({ license: e.target.value })}
                />
              </label>
            </div>
            {c.preset === 'kenney' && (
              <label className={styles.check}>
                <input
                  type="checkbox"
                  checked={c.lookupStorePage}
                  onChange={(e) => set({ lookupStorePage: e.target.checked })}
                />
                Look up the store page on kenney.nl (one or two HEAD requests at build time)
              </label>
            )}
          </div>
          <div className={styles.section}>
            <span className={styles.sectionLabel}>Resulting folders</span>
            {c.tree && c.treeKey === key ? (
              <Tree tree={c.tree} converts={c.converts && !convertOff} />
            ) : (
              <span className={styles.hint}>
                {c.variants.length > 0 && c.variant === null
                  ? 'Tick a variant to see its folders.'
                  : 'Reading the original…'}
              </span>
            )}
          </div>
        </>
      )}
    </li>
  );
}

/** Import pack: an original's candidate packs, their targets, steps and trees, then Import */
export function ImportDialog() {
  const state = useImportStore((s) => s.dialog);
  const scanning = useImportStore((s) => s.scanning);
  const scanError = useImportStore((s) => s.scanError);
  const draft = useImportStore((s) => s.draft);
  const submitting = useImportStore((s) => s.submitting);
  const { closeDialog, setLibrary, pickOffer, edit, submit } = useImportStore.getState();
  const libraries = useLibraryStore((s) => s.libraries);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      // Taken here, so the Imports view under the dialog stays open
      if (e.key !== 'Escape' || !useImportStore.getState().dialog) return;
      e.preventDefault();
      closeDialog();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [closeDialog]);

  if (!state) return null;
  const problem = draft ? draftProblem(draft) : null;
  const ticked = draft?.candidates.filter((c) => c.ticked && !c.imported).length ?? 0;
  const many = (draft?.candidates.length ?? 0) > 1;

  return (
    <div className={dialog.overlay} onClick={closeDialog}>
      <div
        className={clsx(dialog.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="import-dialog-title"
        id="import-dialog"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconInbox size={16} />
            <h3 id="import-dialog-title" className={dialog.title}>
              Import {originalName(state.originalPath)}
            </h3>
          </div>
          <button
            className={dialog.closeBtn}
            onClick={closeDialog}
            disabled={submitting}
            title="Close"
          >
            <IconX size={14} />
          </button>
        </div>
        <div className={dialog.content}>
          <span className={styles.path}>{state.originalPath}</span>
          <div className={styles.fieldRow}>
            <label className={styles.field}>
              Library
              <select
                className={dialog.input}
                value={state.libraryId}
                onChange={(e) => void setLibrary(e.target.value)}
                disabled={submitting}
              >
                {libraries.map((l) => (
                  <option key={l.id} value={l.id}>
                    {l.name}
                  </option>
                ))}
              </select>
            </label>
            {draft && (
              <label className={styles.field}>
                Publisher
                <input
                  id="import-publisher"
                  className={dialog.input}
                  value={draft.publisher}
                  onChange={(e) => edit((d) => editPublisher(d, e.target.value))}
                  disabled={submitting}
                />
              </label>
            )}
          </div>
          {scanning && <p className={styles.hint}>Reading the original…</p>}
          {scanError && <p className={dialog.error}>{scanError}</p>}
          {draft && draft.offers.length > 1 && (
            <div className={styles.section}>
              <span className={styles.sectionLabel}>This zip matches several layouts</span>
              <div className={styles.offers}>
                {draft.offers.map((o, i) => (
                  <label
                    key={o.layout}
                    className={clsx(styles.offer, draft.offer === i && styles.offerSelected)}
                  >
                    <input
                      type="radio"
                      name="import-offer"
                      checked={draft.offer === i}
                      onChange={() => pickOffer(i)}
                      disabled={submitting}
                    />
                    {OFFER_LABELS[o.layout]}
                    {o.candidates.length > 1 && ` (${o.candidates.length} packs)`}
                  </label>
                ))}
              </div>
            </div>
          )}
          {/* Import sends the packs as they are when it starts, so nothing changes until it ends */}
          <fieldset className={styles.lock} disabled={submitting}>
            {draft && many && (
              <div className={styles.footer}>
                <span className={styles.hint}>
                  {plural(ticked, 'pack')} of {draft.candidates.length} ticked
                </span>
                <button
                  className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
                  onClick={() =>
                    edit((d) => ({
                      ...d,
                      candidates: d.candidates.map((c) =>
                        c.imported ? c : { ...c, ticked: true }
                      ),
                    }))
                  }
                >
                  Tick all
                </button>
                <button
                  className={clsx(dialog.btn, dialog.secondaryBtn, styles.smallBtn)}
                  onClick={() =>
                    edit((d) => ({
                      ...d,
                      candidates: d.candidates.map((c) =>
                        c.imported ? c : { ...c, ticked: false }
                      ),
                    }))
                  }
                >
                  Untick all
                </button>
              </div>
            )}
            {draft && (
              <ul className={styles.list}>
                {draft.candidates.map((_, i) => (
                  <Candidate
                    key={`${draft.offer}/${i}`}
                    draft={draft}
                    index={i}
                    expandedByDefault={!many}
                  />
                ))}
              </ul>
            )}
          </fieldset>
        </div>
        <div className={dialog.actions}>
          <span className={styles.footerNote} data-import-problem>
            {draft && !submitting ? problem : null}
          </span>
          <button
            className={clsx(dialog.btn, dialog.secondaryBtn)}
            onClick={closeDialog}
            disabled={submitting}
          >
            Close
          </button>
          <button
            id="import-submit"
            className={clsx(dialog.btn, dialog.primaryBtn)}
            disabled={!draft || problem !== null || submitting}
            onClick={() => void submit()}
          >
            {submitting ? 'Importing…' : ticked > 1 ? `Import ${ticked} packs` : 'Import'}
          </button>
        </div>
      </div>
    </div>
  );
}
