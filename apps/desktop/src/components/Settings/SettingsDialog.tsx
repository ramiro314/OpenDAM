import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import type {
  AiProvider,
  AiSettings,
  AnthropicAccess,
  ClaudePlanStatus,
  ConnectionTest,
  ProviderStatus,
} from '../../types';
import { formatCost } from '../../utils/autotag';
import { IconSettings, IconX } from '../Icons';
import {
  ANTHROPIC_MODELS,
  claudePlanLines,
  ignoredOverridesNote,
  isKnownModel,
  parseNumberInput,
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
    if (!patch) {
      onClose();
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await api.setAiSettings(patch);
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
              </section>
            </>
          )}

          {error && (
            <p className={styles.error} role="alert">
              {error}
            </p>
          )}
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
  /** The model in the form, which may not be saved yet */
  model: string;
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
