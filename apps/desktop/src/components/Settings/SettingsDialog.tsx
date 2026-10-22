import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import { api } from '../../api';
import { useCollectionStore } from '../../store';
import type {
  AiProvider,
  AiSettings,
  AnthropicAccess,
  ClaudePlanStatus,
  ConnectionTest,
  ExportSettings,
  ProviderStatus,
} from '../../types';
import { formatCost } from '../../utils/autotag';
import { IconSettings, IconX } from '../Icons';
import {
  ANTHROPIC_MODELS,
  claudePlanLines,
  exportSettingsPatch,
  ignoredOverridesNote,
  isKnownModel,
  parseNumberInput,
  rootCollectionOptions,
  settingsPatch,
} from './settingsHelpers';
import styles from './SettingsDialog.module.css';

interface SettingsDialogProps {
  onClose: () => void;
}

const CUSTOM_MODEL = 'custom';

export function SettingsDialog({ onClose }: SettingsDialogProps) {
  const [saved, setSaved] = useState<AiSettings | null>(null);
  const [draft, setDraft] = useState<AiSettings | null>(null);
  const [customModel, setCustomModel] = useState(false);
  const [statuses, setStatuses] = useState<ProviderStatus[]>([]);
  const [claudeStatus, setClaudeStatus] = useState<ClaudePlanStatus | null>(null);
  const [claudeError, setClaudeError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [exportSaved, setExportSaved] = useState<ExportSettings | null>(null);
  const [exportDraft, setExportDraft] = useState<ExportSettings | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    Promise.all([api.getAiSettings(), api.getProviderStatus()])
      .then(([settings, status]) => {
        setSaved(settings);
        setDraft(settings);
        setCustomModel(!isKnownModel(settings.anthropic.model));
        setStatuses(status);
      })
      .catch((e) => setError(String(e)));
    api
      .getExportSettings()
      .then((settings) => {
        setExportSaved(settings);
        setExportDraft(settings);
      })
      .catch((e) => setExportError(String(e)));
  }, []);

  // Runs `claude` twice, so it doesn't hold up the rest of the dialog
  const checkClaude = () => {
    setClaudeStatus(null);
    setClaudeError(null);
    api
      .getClaudePlanStatus()
      .then(setClaudeStatus)
      .catch((e) => setClaudeError(String(e)));
  };
  useEffect(checkClaude, []);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  const statusOf = (provider: AiProvider) => statuses.find((s) => s.provider === provider);
  const updateStatus = (status: ProviderStatus) =>
    setStatuses((all) => all.map((s) => (s.provider === status.provider ? status : s)));

  const setAnthropic = (patch: Partial<AiSettings['anthropic']>) =>
    setDraft((d) => d && { ...d, anthropic: { ...d.anthropic, ...patch } });
  const setJev = (patch: Partial<AiSettings['jev']>) =>
    setDraft((d) => d && { ...d, jev: { ...d.jev, ...patch } });

  const handleSave = async () => {
    if (!saved || !draft) return;
    const trimmed = {
      ...draft,
      anthropic: { ...draft.anthropic, model: draft.anthropic.model.trim() },
    };
    const patch = settingsPatch(saved, trimmed);
    const exportPatch =
      exportSaved && exportDraft ? exportSettingsPatch(exportSaved, exportDraft) : null;
    if (!patch && !exportPatch) {
      onClose();
      return;
    }
    setSaving(true);
    setError(null);
    setExportError(null);
    // The export root can be refused, so it saves first and keeps the dialog open when it is
    if (exportPatch) {
      try {
        setExportSaved(await api.setExportSettings(exportPatch));
      } catch (e) {
        setExportError(String(e));
        setSaving(false);
        return;
      }
    }
    try {
      if (patch) await api.setAiSettings(patch);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const modelSelectValue = customModel ? CUSTOM_MODEL : draft?.anthropic.model;
  const onPlan = draft?.anthropic.access === 'claude_plan';
  const anthropicReady = onPlan
    ? (claudeStatus?.configured ?? false)
    : (statusOf('anthropic')?.configured ?? false);

  return (
    <div className={styles.overlay} onClick={onClose}>
      <div
        className={styles.dialog}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
      >
        <div className={styles.header}>
          <div className={styles.titleArea}>
            <IconSettings size={16} />
            <h3 id="settings-title" className={styles.title}>
              Settings
            </h3>
          </div>
          <button className={styles.closeBtn} onClick={onClose} title="Close">
            <IconX size={14} />
          </button>
        </div>

        <div className={styles.content}>
          <h4 className={styles.sectionTitle}>AI tagging</h4>

          {draft && (
            <>
              <section className={styles.provider}>
                <div className={styles.providerHeader}>
                  <span className={styles.providerName}>Anthropic</span>
                </div>
                <label className={styles.row}>
                  <span className={styles.label}>Access</span>
                  <select
                    id="settings-anthropic-access"
                    className={styles.input}
                    value={draft.anthropic.access}
                    onChange={(e) => setAnthropic({ access: e.target.value as AnthropicAccess })}
                  >
                    <option value="api_key">API key</option>
                    <option value="claude_plan">Claude plan (your installed claude)</option>
                  </select>
                </label>
                {onPlan ? (
                  <ClaudePlanField
                    status={claudeStatus}
                    error={claudeError}
                    onCheckAgain={checkClaude}
                  />
                ) : (
                  <ApiKeyField
                    provider="anthropic"
                    status={statusOf('anthropic')}
                    onStatus={updateStatus}
                  />
                )}

                <label className={styles.row}>
                  <span className={styles.label}>Model</span>
                  <select
                    id="settings-anthropic-model"
                    className={styles.input}
                    value={modelSelectValue}
                    onChange={(e) => {
                      if (e.target.value === CUSTOM_MODEL) {
                        setCustomModel(true);
                      } else {
                        setCustomModel(false);
                        setAnthropic({ model: e.target.value });
                      }
                    }}
                  >
                    {ANTHROPIC_MODELS.map((m) => (
                      <option key={m.id} value={m.id}>
                        {m.label}
                      </option>
                    ))}
                    <option value={CUSTOM_MODEL}>Custom model id…</option>
                  </select>
                </label>
                {customModel && (
                  <label className={styles.row}>
                    <span className={styles.label}>Model id</span>
                    <input
                      id="settings-anthropic-custom-model"
                      className={styles.input}
                      value={draft.anthropic.model}
                      placeholder="claude-…"
                      spellCheck={false}
                      onChange={(e) => setAnthropic({ model: e.target.value })}
                    />
                  </label>
                )}
                <div className={styles.numbers}>
                  <NumberField
                    label="Requests in flight"
                    value={draft.anthropic.concurrency}
                    min={1}
                    max={16}
                    onChange={(concurrency) => setAnthropic({ concurrency })}
                  />
                  <NumberField
                    label="Max tags per asset"
                    value={draft.anthropic.maxTags}
                    min={1}
                    max={50}
                    onChange={(maxTags) => setAnthropic({ maxTags })}
                  />
                  <NumberField
                    label="Max new-tag proposals"
                    value={draft.anthropic.maxProposals}
                    min={0}
                    max={20}
                    onChange={(maxProposals) => setAnthropic({ maxProposals })}
                  />
                </div>
                <TestConnection
                  provider="anthropic"
                  model={draft.anthropic.model.trim()}
                  access={draft.anthropic.access}
                  configured={anthropicReady}
                />
              </section>

              <section className={styles.provider}>
                <div className={styles.providerHeader}>
                  <span className={styles.providerName}>Jev</span>
                  <span className={styles.providerNote}>TypeSafe AI, text only</span>
                </div>
                <ApiKeyField provider="jev" status={statusOf('jev')} onStatus={updateStatus} />
                <div className={styles.numbers}>
                  <NumberField
                    label="Requests in flight"
                    value={draft.jev.concurrency}
                    min={1}
                    max={16}
                    onChange={(concurrency) => setJev({ concurrency })}
                  />
                  <NumberField
                    label="Score threshold"
                    value={draft.jev.threshold}
                    min={0}
                    max={1}
                    step={0.05}
                    onChange={(threshold) => setJev({ threshold })}
                  />
                  <NumberField
                    label="Max tags per asset"
                    value={draft.jev.maxTags}
                    min={1}
                    max={50}
                    onChange={(maxTags) => setJev({ maxTags })}
                  />
                </div>
                <TestConnection provider="jev" configured={statusOf('jev')?.configured ?? false} />
              </section>
            </>
          )}

          {error && (
            <p className={styles.error} role="alert">
              {error}
            </p>
          )}

          <h4 className={styles.sectionTitle}>Game export</h4>
          <ExportSection draft={exportDraft} onChange={setExportDraft} error={exportError} />
        </div>

        <div className={styles.actions}>
          <button type="button" className={clsx(styles.btn, styles.secondaryBtn)} onClick={onClose}>
            Cancel
          </button>
          <button
            id="settings-save"
            type="button"
            className={clsx(styles.btn, styles.primaryBtn)}
            disabled={!draft || saving}
            onClick={handleSave}
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}

interface ExportSectionProps {
  draft: ExportSettings | null;
  onChange: (draft: ExportSettings) => void;
  error: string | null;
}

/**
 * The export root and root collection, saved with the dialog, and "Clear export cache", which
 * acts at once. The root is checked against every library when it is saved.
 */
function ExportSection({ draft, onChange, error }: ExportSectionProps) {
  const collections = useCollectionStore((s) => s.collections);
  const options = rootCollectionOptions(collections);
  const [clearing, setClearing] = useState(false);
  const [cleared, setCleared] = useState<string | null>(null);
  const [clearError, setClearError] = useState<string | null>(null);

  const chooseRoot = async () => {
    const picked = await open({ directory: true, multiple: false, title: 'Select Export Root' });
    if (draft && typeof picked === 'string') onChange({ ...draft, root: picked });
  };

  const clearCache = async () => {
    setClearing(true);
    setCleared(null);
    setClearError(null);
    try {
      const count = await api.clearExportCache();
      setCleared(
        count === 0
          ? 'No library had an export cache'
          : `Cleared the export cache of ${count} ${count === 1 ? 'library' : 'libraries'}`
      );
    } catch (e) {
      setClearError(String(e));
    } finally {
      setClearing(false);
    }
  };

  const missing =
    draft?.rootCollection != null && !options.some((c) => c.id === draft.rootCollection);

  return (
    <section className={styles.provider} id="settings-export">
      {draft && (
        <>
          <div className={styles.row}>
            <span className={styles.label}>Export root</span>
            <input
              id="settings-export-root"
              className={styles.input}
              value={draft.root ?? ''}
              placeholder="No folder chosen"
              readOnly
            />
            <button
              type="button"
              className={clsx(styles.btn, styles.secondaryBtn)}
              onClick={chooseRoot}
            >
              Choose…
            </button>
            <button
              type="button"
              className={clsx(styles.btn, styles.secondaryBtn)}
              disabled={!draft.root}
              onClick={() => onChange({ ...draft, root: null })}
            >
              Clear
            </button>
          </div>
          <p className={styles.keyStatus}>
            <span className={styles.hint}>A folder outside every library</span>
          </p>
          <label className={styles.row}>
            <span className={styles.label}>Root collection</span>
            <select
              id="settings-export-root-collection"
              className={styles.input}
              value={draft.rootCollection ?? ''}
              onChange={(e) => onChange({ ...draft, rootCollection: e.target.value || null })}
            >
              <option value="">None</option>
              {missing && <option value={draft.rootCollection ?? ''}>Missing collection</option>}
              {options.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </label>
          <p className={styles.keyStatus}>
            <span className={styles.hint}>
              A top-level collection whose subcollections are the export domains
            </span>
          </p>
        </>
      )}
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}
      <div className={styles.keyBlock}>
        <div className={styles.row}>
          <span className={styles.label} />
          <button
            id="settings-export-clear-cache"
            type="button"
            className={clsx(styles.btn, styles.secondaryBtn)}
            disabled={clearing}
            onClick={clearCache}
            title="Delete every library's .opendam/export-cache folder"
          >
            {clearing ? 'Clearing…' : 'Clear export cache'}
          </button>
        </div>
        {cleared && (
          <p className={styles.keyStatus} role="status">
            {cleared}
          </p>
        )}
        {clearError && (
          <p className={styles.error} role="alert">
            {clearError}
          </p>
        )}
      </div>
    </section>
  );
}

interface NumberFieldProps {
  label: string;
  value: number;
  min: number;
  max: number;
  step?: number;
  onChange: (value: number) => void;
}

/**
 * Keeps what the user types, so emptying the field to retype it doesn't turn into 0. Only a
 * valid number reaches `onChange` (a whole one unless `step` is fractional), and leaving the
 * field shows the value that will be saved again.
 */
function NumberField({ label, value, min, max, step = 1, onChange }: NumberFieldProps) {
  const integer = Number.isInteger(step);
  const [text, setText] = useState(String(value));
  // Follow outside changes, but keep a partial entry such as "0." that already means `value`
  useEffect(
    () => setText((t) => (parseNumberInput(t, integer) === value ? t : String(value))),
    [value, integer]
  );

  return (
    <label className={styles.numberField}>
      <span className={styles.label}>{label}</span>
      <input
        type="number"
        className={styles.input}
        value={text}
        min={min}
        max={max}
        step={step}
        onChange={(e) => {
          setText(e.target.value);
          const parsed = parseNumberInput(e.target.value, integer);
          if (parsed !== null) onChange(parsed);
        }}
        onBlur={() => setText(String(value))}
      />
    </label>
  );
}

interface ApiKeyFieldProps {
  provider: AiProvider;
  status: ProviderStatus | undefined;
  onStatus: (status: ProviderStatus) => void;
}

/** Write-only: the field starts empty and is cleared once the key is saved. */
function ApiKeyField({ provider, status, onStatus }: ApiKeyFieldProps) {
  const [key, setKey] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (value: string | null) => {
    setBusy(true);
    setError(null);
    try {
      onStatus(await api.setApiKey(provider, value));
      setKey('');
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={styles.keyBlock}>
      <div className={styles.row}>
        <span className={styles.label}>API key</span>
        <input
          id={`settings-${provider}-key`}
          type="password"
          className={styles.input}
          value={key}
          placeholder={status?.savedInKeyring ? 'Enter a new key to replace it' : 'Paste a key'}
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => setKey(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !busy && key.trim()) submit(key);
          }}
        />
        <button
          type="button"
          className={clsx(styles.btn, styles.secondaryBtn)}
          disabled={busy || !key.trim()}
          onClick={() => submit(key)}
        >
          Save
        </button>
        <button
          type="button"
          className={clsx(styles.btn, styles.secondaryBtn)}
          disabled={busy || !status?.savedInKeyring}
          onClick={() => submit(null)}
          title="Remove the key saved in the keyring"
        >
          Clear
        </button>
      </div>
      {status && (
        <p className={styles.keyStatus}>
          <span
            className={clsx(styles.dot, status.configured ? styles.dotOn : styles.dotOff)}
            aria-hidden
          />
          Key {status.sourceLabel}
          {status.keySource === 'env' && status.savedInKeyring && ' (overrides the saved key)'}
          {status.keySource !== 'env' && (
            <span className={styles.hint}> · {status.envVar} overrides a saved key</span>
          )}
        </p>
      )}
      {status?.keyringProblem && <p className={styles.warning}>{status.keyringProblem}</p>}
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

interface ClaudePlanFieldProps {
  status: ClaudePlanStatus | null;
  error: string | null;
  onCheckAgain: () => void;
}

/**
 * Where `claude` is, its version and its login, read with the same cleaned environment runs
 * use, so an ANTHROPIC_API_KEY set for the app can't make it look like the plan is in use.
 */
function ClaudePlanField({ status, error, onCheckAgain }: ClaudePlanFieldProps) {
  const lines = status && claudePlanLines(status);
  const overrides = status && ignoredOverridesNote(status.ignoredOverrides);
  return (
    <div className={styles.keyBlock} id="settings-claude-plan">
      <div className={styles.row}>
        <span className={styles.label}>Claude CLI</span>
        <span className={styles.cliStatus}>
          {!status && !error && 'Checking claude…'}
          {lines && (
            <>
              <span className={styles.cliLine}>{lines.found}</span>
              <span className={styles.cliLine}>
                <span
                  className={clsx(styles.dot, status.configured ? styles.dotOn : styles.dotOff)}
                  aria-hidden
                />
                {lines.login}
              </span>
            </>
          )}
        </span>
        <button
          type="button"
          className={clsx(styles.btn, styles.secondaryBtn)}
          disabled={!status && !error}
          onClick={onCheckAgain}
          title="Ask claude for its login again"
        >
          Check again
        </button>
      </div>
      {status?.problem && <p className={styles.warning}>{status.problem}</p>}
      {overrides && <p className={styles.keyStatus}>{overrides}</p>}
      <p className={styles.keyStatus}>
        <span className={styles.hint}>
          Personal use only: OpenDAM runs your own logged-in claude, and its requests count against
          your plan&apos;s limits.
        </span>
      </p>
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

interface TestConnectionProps {
  provider: AiProvider;
  /** The model in the form, which may not be saved yet. Jev has none and tests with the saved
   * threshold and cap. */
  model?: string;
  /** The access in the form, for Anthropic */
  access?: AnthropicAccess;
  configured: boolean;
}

/** Sends one small tagging request with the configured key, or on your Claude plan. */
function TestConnection({ provider, model, access, configured }: TestConnectionProps) {
  const onPlan = access === 'claude_plan';
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<ConnectionTest | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    setBusy(true);
    setResult(null);
    setError(null);
    try {
      setResult(await api.testProvider(provider, model || undefined, access));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={styles.keyBlock}>
      <div className={styles.row}>
        <span className={styles.label} />
        <button
          id={`settings-${provider}-test`}
          type="button"
          className={clsx(styles.btn, styles.secondaryBtn)}
          disabled={busy || !configured}
          onClick={run}
          title={
            onPlan
              ? configured
                ? 'Send one small request through claude, on your plan'
                : 'Log claude in to your Claude plan first'
              : configured
                ? 'Send one small request with this key'
                : 'Save an API key first'
          }
        >
          {busy ? 'Testing…' : 'Test connection'}
        </button>
      </div>
      {result && (
        <p
          id={`settings-${provider}-test-result`}
          className={result.ok ? styles.keyStatus : styles.error}
          role="status"
        >
          {result.ok && <span className={clsx(styles.dot, styles.dotOn)} aria-hidden />}
          <span>
            {result.message}
            {result.ok && (
              <span className={styles.hint}>
                {' '}
                · {(result.inputTokens + result.outputTokens).toLocaleString()} tokens ·{' '}
                {result.onPlan ? 'on your plan' : formatCost(result.costUsd)}
              </span>
            )}
          </span>
        </p>
      )}
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
