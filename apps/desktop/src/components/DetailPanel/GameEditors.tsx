import { useEffect, useMemo, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import type {
  ExportEditPatch,
  GameImage,
  GameMaterial,
  GameModelDetail,
  GameRig,
  GameTexture,
  MaterialSurface,
  NormalConvention,
  TextureLayoutEdit,
  TextureRole,
} from '../../types';
import {
  exportedView,
  facingLine,
  materialNameError,
  pickerOptions,
  rigPatch,
  roleEditable,
  shortPaths,
  shownValue,
  slotPatch,
  sourcePatch,
  turnPatch,
  viewKey,
  withPatch,
  type ViewResult,
} from '../../utils/gameTab';
import { IconRotateCcw, IconRotateCw } from '../Icons';
import { ThreeViewer } from './ThreeViewer';
import styles from './DetailPanel.module.css';

const ROLES: TextureRole[] = [
  'BaseColor',
  'Normal',
  'MetallicSmoothness',
  'Occlusion',
  'Emission',
  'Height',
];

const LAYOUTS: [TextureLayoutEdit, string][] = [
  ['passThrough', 'One role'],
  ['ORM', 'ORM (AO, roughness, metallic)'],
  ['MaskMap', 'HDRP MaskMap'],
  ['Roughness', 'Roughness'],
  ['Gloss', 'Gloss / smoothness'],
  ['Metallic', 'Metallic'],
];

const CONVENTIONS: [NormalConvention, string][] = [
  ['opengl', 'OpenGL (green up)'],
  ['directx', 'DirectX (green down)'],
];

const SELECT = clsx(styles.packFormInput, styles.packFormSelect, styles.gameSelect);

interface EditorProps {
  saving: boolean;
  save: (patch: ExportEditPatch) => void;
}

/** A label above its control, the value's inferred-or-yours mark, and its reset */
function Row({
  label,
  id,
  yours,
  saving,
  onReset,
  children,
}: {
  label: string;
  id: string;
  yours: boolean;
  saving: boolean;
  onReset: () => void;
  children: React.ReactNode;
}) {
  return (
    <div className={styles.gameRow}>
      <label className={styles.infoLabel} htmlFor={id}>
        {label}
      </label>
      <div className={styles.gameValue}>
        {children}
        <span
          id={`${id}-mark`}
          className={clsx(styles.gameMark, yours && styles.gameMarkYours)}
          title={yours ? 'You set this' : 'Inferred: choose your own to replace it'}
        >
          {yours ? 'yours' : 'inferred'}
        </span>
        <button
          type="button"
          id={`${id}-reset`}
          className={styles.openLicenseBtn}
          onClick={onReset}
          disabled={saving || !yours}
          title="Go back to the inferred value"
        >
          Reset
        </button>
      </div>
    </div>
  );
}

/**
 * A model's rig and facing (plan game-export C13, D13, D20): a rig picker and quarter turns
 * about Y, each marked inferred or yours with a reset, and "Show as exported", which has Blender
 * write the normalized model for the viewer only when you ask. Once shown, the view asks again
 * after each edit (`revision`), so it follows each turn. A turn is computed from the value the
 * last save wrote, so the controls wait while the tab reads that save back (`stale`), showing
 * the saved value meanwhile.
 */
export function ModelEditor({
  assetId,
  model: read,
  revision,
  stale,
  saving,
  save,
}: Omit<EditorProps, 'save'> & {
  assetId: string;
  model: GameModelDetail;
  /** Counts the tab's saved edits: each makes an open view ask again */
  revision: number;
  /** Whether the tab is still reading back the last save, which `model` may not show yet */
  stale: boolean;
  /** Resolves whether the edit was written */
  save: (patch: ExportEditPatch) => Promise<boolean>;
}) {
  // The last save's values, until the detail read after it arrives
  const [pending, setPending] = useState<ExportEditPatch | null>(null);
  useEffect(() => setPending(null), [read]);
  const model = withPatch(read, pending);
  const busy = saving || stale;
  const apply = (patch: ExportEditPatch) => {
    setPending((p) => ({ ...p, ...patch }));
    void save(patch).then((written) => written || setPending(null));
  };
  const rig = shownValue(model.rig);
  const facing = shownValue(model.facingQuarterTurns);
  const [showing, setShowing] = useState(false);
  const [result, setResult] = useState<ViewResult | null>(null);
  const key = viewKey(assetId, revision);
  const view = exportedView(showing, result, key);
  useEffect(() => {
    if (!showing || result?.key === key) return;
    let current = true;
    api
      .showAsExported(assetId)
      .then((path) => current && setResult({ key, path }))
      .catch((err) => current && setResult({ key, error: String(err) }));
    return () => {
      current = false;
    };
  }, [showing, result, key, assetId]);

  return (
    <section className={styles.gameSection} id="detail-game-model">
      <h4 className={styles.gameHeading}>Rig and facing</h4>
      <Row
        label="Rig"
        id="detail-game-rig"
        yours={rig.mark === 'yours'}
        saving={busy}
        onReset={() => apply({ rig: null })}
      >
        <select
          id="detail-game-rig"
          className={SELECT}
          value={rig.value}
          disabled={busy}
          title="Inferred: none without an armature, else generic"
          onChange={(e) => {
            const patch = rigPatch(model.rig, e.target.value as GameRig);
            if (patch) apply(patch);
          }}
        >
          <option value="none">None</option>
          <option value="generic">Generic</option>
        </select>
      </Row>
      <Row
        label="Facing"
        id="detail-game-facing"
        yours={facing.mark === 'yours'}
        saving={busy}
        onReset={() => apply({ facingQuarterTurns: null })}
      >
        <button
          type="button"
          id="detail-game-facing"
          className={styles.openLicenseBtn}
          onClick={() => apply(turnPatch(model.facingQuarterTurns, -1))}
          disabled={busy}
          title="A quarter turn counterclockwise, seen from above"
          aria-label="A quarter turn counterclockwise, seen from above"
        >
          <IconRotateCcw size={12} />
        </button>
        <button
          type="button"
          id="detail-game-facing-right"
          className={styles.openLicenseBtn}
          onClick={() => apply(turnPatch(model.facingQuarterTurns, 1))}
          disabled={busy}
          title="A quarter turn clockwise, seen from above: +Z to +X"
          aria-label="A quarter turn clockwise, seen from above: +Z to +X"
        >
          <IconRotateCw size={12} />
        </button>
      </Row>
      <div className={styles.gameSuggestion}>
        <span className={styles.gameHint} id="detail-game-facing-line">
          {facingLine(model)}.
        </span>
        <button
          type="button"
          id="detail-game-show-exported"
          className={clsx(styles.openLicenseBtn, showing && styles.actionBtnActive)}
          onClick={() => {
            // Asking again retries a failure, as after setting Blender's path in Settings
            if (!showing && result && 'error' in result) setResult(null);
            setShowing(!showing);
          }}
          title="Blender writes the model as the next export does, facing the blue arrow's +Z"
        >
          {showing ? 'Hide' : 'Show as exported'}
        </button>
      </div>
      {view.status === 'loading' && (
        <p className={styles.gameHint}>Normalizing it in Blender as the export will…</p>
      )}
      {view.status === 'failed' && (
        <p className={styles.packFormError} role="alert" id="detail-game-view-error">
          {view.error}
        </p>
      )}
      {view.status === 'shown' && (
        <div className={styles.gameViewer} id="detail-game-viewer">
          <ThreeViewer modelPath={view.path} forwardArrow />
        </div>
      )}
    </section>
  );
}

/**
 * A model's material slots (plan C7, C13): each slot's name, surface and a texture per role,
 * each marked inferred or yours, with a reset. A role's picker offers the pack's images that
 * give it, or none.
 */
export function MaterialEditor({
  materials,
  images,
  saving,
  save,
}: EditorProps & { materials: GameMaterial[]; images: GameImage[] }) {
  const paths = useMemo(() => shortPaths(images), [images]);
  return (
    <section className={styles.gameSection} id="detail-game-materials">
      <h4 className={styles.gameHeading}>Materials</h4>
      {materials.map((m, i) => (
        <MaterialSlot
          key={m.slot}
          id={`detail-game-material-${i}`}
          material={m}
          images={images}
          paths={paths}
          saving={saving}
          save={save}
        />
      ))}
    </section>
  );
}

function MaterialSlot({
  material: m,
  images,
  paths,
  id,
  saving,
  save,
}: EditorProps & {
  material: GameMaterial;
  images: GameImage[];
  paths: Map<string, string>;
  id: string;
}) {
  const name = shownValue(m.name);
  const surface = shownValue(m.surface);
  const [draft, setDraft] = useState(name.value);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => setDraft(name.value), [name.value]);

  // The one commit of a name: Enter blurs the input, so blur is the only caller, and a save in
  // flight (which disables the input, blurring it again) isn't sent twice
  const commitName = () => {
    const value = draft.trim();
    if (saving || value === name.value) return;
    const problem = materialNameError(value);
    if (problem) setError(problem);
    else save(slotPatch(m, { field: 'name', value }));
  };

  return (
    <fieldset className={styles.gameSlot} id={id}>
      <legend className={styles.gameSlotName}>
        {m.slot}
        {m.added && <span className={styles.gameHint}> · added at export</span>}
      </legend>
      <Row
        label="Name"
        id={`${id}-name`}
        yours={name.mark === 'yours'}
        saving={saving}
        onReset={() => save(slotPatch(m, { field: 'name', value: null }))}
      >
        <input
          id={`${id}-name`}
          className={styles.packFormInput}
          value={draft}
          spellCheck={false}
          disabled={saving}
          onChange={(e) => {
            setDraft(e.target.value);
            setError(null);
          }}
          onBlur={commitName}
          onKeyDown={(e) => e.key === 'Enter' && e.currentTarget.blur()}
        />
      </Row>
      {error && (
        <p className={styles.packFormError} role="alert">
          {error}
        </p>
      )}
      <Row
        label="Surface"
        id={`${id}-surface`}
        yours={surface.mark === 'yours'}
        saving={saving}
        onReset={() => save(slotPatch(m, { field: 'surface', value: null }))}
      >
        <select
          id={`${id}-surface`}
          className={SELECT}
          value={surface.value}
          disabled={saving}
          onChange={(e) =>
            save(slotPatch(m, { field: 'surface', value: e.target.value as MaterialSurface }))
          }
        >
          <option value="opaque">Opaque</option>
          <option value="cutout">Cutout</option>
        </select>
      </Row>
      {m.cutoutSuggested && surface.value === 'opaque' && (
        <div className={styles.gameSuggestion}>
          <span className={styles.gameHint}>
            Its base color has a cut-out alpha. Until you accept it, it exports opaque.
          </span>
          <button
            type="button"
            id={`${id}-accept-cutout`}
            className={styles.openLicenseBtn}
            disabled={saving}
            onClick={() => save(slotPatch(m, { field: 'surface', value: 'cutout' }))}
          >
            Use cutout
          </button>
        </div>
      )}
      {m.textures.map((t) => {
        const current = t.value?.assetId ?? t.picked;
        const options = pickerOptions(images, t.role, current);
        // A pick the export can't use: its image left the pack
        const missing = current !== null && !options.some((o) => o.assetId === current);
        return (
          <Row
            key={t.role}
            label={t.role}
            id={`${id}-${t.role}`}
            yours={t.yours}
            saving={saving}
            onReset={() => save(slotPatch(m, { field: 'texture', role: t.role, value: undefined }))}
          >
            <select
              id={`${id}-${t.role}`}
              className={SELECT}
              value={current ?? ''}
              disabled={saving}
              title={t.value ? `${t.value.assetName}: ${t.value.files.join(', ')}` : 'None'}
              onChange={(e) =>
                save(
                  slotPatch(m, { field: 'texture', role: t.role, value: e.target.value || null })
                )
              }
            >
              <option value="">None</option>
              {missing && <option value={current}>Missing image: pick another</option>}
              {options.map((o) => (
                <option key={o.assetId} value={o.assetId}>
                  {paths.get(o.assetId) ?? o.filePath}
                </option>
              ))}
            </select>
          </Row>
        );
      })}
    </fieldset>
  );
}

/**
 * A texture source (plan C8, C13): its set's sources and the roles it exports, and the source's
 * own layout, its role when the layout passes one role through, and a normal map's convention,
 * each marked inferred or yours, with a reset.
 */
export function TextureEditor({ texture, saving, save }: EditorProps & { texture: GameTexture }) {
  const layout = shownValue(texture.layout);
  const roleField = roleEditable(layout.value) ? texture.role : null;
  const role = roleField && shownValue(roleField);
  const convention = shownValue(texture.normalConvention);
  return (
    <section className={styles.gameSection} id="detail-game-texture">
      <h4 className={styles.gameHeading}>Texture set</h4>
      <ul className={styles.gameSources} id="detail-game-texture-set">
        {texture.set.map((s) => (
          <li key={s.assetId}>
            <span className={styles.gameSourcePath}>{s.filePath}</span>
            <span className={styles.gameHint}>{s.layout}</span>
          </li>
        ))}
      </ul>
      <p className={styles.gameHint} id="detail-game-texture-outputs">
        {texture.outputs.length > 0
          ? `Exports ${texture.outputs.join(', ')}`
          : 'Exports no texture'}
      </p>
      <Row
        label="Layout"
        id="detail-game-layout"
        yours={layout.mark === 'yours'}
        saving={saving}
        onReset={() => save({ layout: null })}
      >
        <select
          id="detail-game-layout"
          className={SELECT}
          value={layout.value}
          disabled={saving}
          onChange={(e) =>
            save(sourcePatch('layout', texture.layout, e.target.value as TextureLayoutEdit))
          }
        >
          {LAYOUTS.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
      </Row>
      {roleField && role ? (
        <Row
          label="Role"
          id="detail-game-role"
          yours={role.mark === 'yours'}
          saving={saving}
          onReset={() => save({ role: null })}
        >
          <select
            id="detail-game-role"
            className={SELECT}
            value={role.value}
            disabled={saving}
            onChange={(e) => save(sourcePatch('role', roleField, e.target.value as TextureRole))}
          >
            {ROLES.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
        </Row>
      ) : (
        <p className={styles.gameHint} id="detail-game-role-fixed">
          Its layout fixes its roles: pick “One role” to choose one
        </p>
      )}
      {role?.value === 'Normal' && (
        <Row
          label="Normal map"
          id="detail-game-convention"
          yours={convention.mark === 'yours'}
          saving={saving}
          onReset={() => save({ normalConvention: null })}
        >
          <select
            id="detail-game-convention"
            className={SELECT}
            value={convention.value}
            disabled={saving}
            onChange={(e) =>
              save(
                sourcePatch(
                  'normalConvention',
                  texture.normalConvention,
                  e.target.value as NormalConvention
                )
              )
            }
          >
            {CONVENTIONS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </Row>
      )}
    </section>
  );
}
