import { useEffect, useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import { useAutotagStore } from '../../store';
import type { AutotagMode, AutotagTarget } from '../../types';
import { modeOptions, scopeFor } from '../../utils/autotag';
import { IconTag, IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Autotag.module.css';

interface AutotagDialogProps {
  target: AutotagTarget;
  onClose: () => void;
}

/**
 * Picks a provider and a mode, then starts a background run over the target's assets: chosen
 * assets, or a library's untagged assets unless "All assets" is ticked.
 */
export function AutotagDialog({ target, onClose }: AutotagDialogProps) {
  const providers = useAutotagStore((s) => s.providers);
  const fetchProviders = useAutotagStore((s) => s.fetchProviders);
  const setRunsPanelOpen = useAutotagStore((s) => s.setRunsPanelOpen);
  const fetchLastUsed = useAutotagStore((s) => s.fetchLastUsed);
  const [providerId, setProviderId] = useState<string | null>(null);
  const [mode, setMode] = useState<AutotagMode>('existing_only');
  const [allAssets, setAllAssets] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetchProviders().catch((e) => setError(String(e)));
  }, [fetchProviders]);

  // Default to the first provider that can run
  useEffect(() => {
    if (providerId === null && providers) {
      setProviderId(providers.find((p) => p.unavailableReason === null)?.id ?? null);
    }
  }, [providers, providerId]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  const provider = providers?.find((p) => p.id === providerId) ?? null;
  const modes = modeOptions(provider);
  const modeAllowed = modes.some((m) => m.mode === mode && m.enabled);

  const handleStart = async () => {
    if (!provider || !modeAllowed) return;
    setStarting(true);
    setError(null);
    try {
      await api.startAutotagRun(scopeFor(target, allAssets), provider.id, mode);
      onClose();
      setRunsPanelOpen(true);
      // The start saved them for "Suggest tags"
      fetchLastUsed().catch(console.error);
    } catch (e) {
      setError(String(e));
    } finally {
      setStarting(false);
    }
  };

  let title: string;
  if (target.kind === 'library') {
    title = `Auto-tag library ${target.libraryName}`;
  } else {
    const count = target.assetIds.length;
    title = `Auto-tag ${count.toLocaleString()} asset${count === 1 ? '' : 's'}`;
  }

  return (
    <div className={dialog.overlay} onClick={onClose}>
      <div
        className={dialog.dialog}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="autotag-title"
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <IconTag size={16} />
            <h3 id="autotag-title" className={dialog.title}>
              {title}
            </h3>
          </div>
          <button className={dialog.closeBtn} onClick={onClose} title="Close">
            <IconX size={14} />
          </button>
        </div>

        <div className={dialog.content}>
          {target.kind === 'library' && (
            <>
              <h4 className={dialog.sectionTitle}>Assets</h4>
              <fieldset className={styles.fieldset}>
                <label className={clsx(styles.choice, allAssets && styles.choiceSelected)}>
                  <input
                    id="autotag-all-assets"
                    type="checkbox"
                    checked={allAssets}
                    onChange={(e) => setAllAssets(e.target.checked)}
                  />
                  <span className={styles.choiceText}>
                    <span className={styles.choiceLabel}>All assets</span>
                    <span className={styles.choiceNote}>
                      {allAssets
                        ? 'Every asset in the library, tagged or not. Existing tags are kept.'
                        : 'Off: only assets with no tag yet.'}
                    </span>
                  </span>
                </label>
              </fieldset>
            </>
          )}

          <h4 className={dialog.sectionTitle}>Provider</h4>
          <fieldset className={styles.fieldset}>
            {providers?.map((p) => {
              const disabled = p.unavailableReason !== null;
              return (
                <label
                  key={p.id}
                  className={clsx(
                    styles.choice,
                    p.id === providerId && styles.choiceSelected,
                    disabled && styles.choiceDisabled
                  )}
                >
                  <input
                    type="radio"
                    name="autotag-provider"
                    value={p.id}
                    checked={p.id === providerId}
                    disabled={disabled}
                    onChange={() => {
                      setProviderId(p.id);
                      if (!p.modes.includes(mode)) setMode('existing_only');
                    }}
                  />
                  <span className={styles.choiceText}>
                    <span className={styles.choiceLabel}>{p.label}</span>
                    {disabled && <span className={styles.choiceReason}>{p.unavailableReason}</span>}
                  </span>
                </label>
              );
            })}
          </fieldset>

          <h4 className={dialog.sectionTitle}>Mode</h4>
          <fieldset className={styles.fieldset}>
            {modes.map((m) => (
              <label
                key={m.mode}
                className={clsx(
                  styles.choice,
                  m.mode === mode && m.enabled && styles.choiceSelected,
                  !m.enabled && styles.choiceDisabled
                )}
              >
                <input
                  type="radio"
                  name="autotag-mode"
                  value={m.mode}
                  checked={m.mode === mode && m.enabled}
                  disabled={!m.enabled}
                  onChange={() => setMode(m.mode)}
                />
                <span className={styles.choiceText}>
                  <span className={styles.choiceLabel}>{m.label}</span>
                  <span className={styles.choiceNote}>{m.description}</span>
                  {m.reason && <span className={styles.choiceReason}>{m.reason}</span>}
                </span>
              </label>
            ))}
          </fieldset>

          {error && <p className={dialog.error}>{error}</p>}
        </div>

        <div className={dialog.actions}>
          <button className={clsx(dialog.btn, dialog.secondaryBtn)} onClick={onClose}>
            Cancel
          </button>
          <button
            id="autotag-start"
            className={clsx(dialog.btn, dialog.primaryBtn)}
            onClick={handleStart}
            disabled={!provider || !modeAllowed || starting}
          >
            {starting ? 'Starting…' : 'Start'}
          </button>
        </div>
      </div>
    </div>
  );
}
