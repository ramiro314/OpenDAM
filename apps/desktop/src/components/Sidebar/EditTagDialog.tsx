import { useEffect, useState } from 'react';
import clsx from 'clsx';
import type { ColorLabel, Tag } from '../../types';
import { useTagStore } from '../../store';
import { IconX } from '../Icons';
import { parentCandidates } from './sidebarHelpers';
import dialogStyles from '../ConfirmDeleteDialog.module.css';
import styles from './EditTagDialog.module.css';

const COLOR_LABELS: ColorLabel[] = [
  'red',
  'orange',
  'yellow',
  'green',
  'teal',
  'blue',
  'purple',
  'pink',
];

interface EditTagDialogProps {
  tag: Tag | null;
  onClose: () => void;
}

export function EditTagDialog({ tag, onClose }: EditTagDialogProps) {
  const tags = useTagStore((s) => s.tags);
  const updateTag = useTagStore((s) => s.updateTag);

  const [name, setName] = useState('');
  const [color, setColor] = useState<ColorLabel | null>(null);
  const [parentId, setParentId] = useState('');
  const [description, setDescription] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!tag) return;
    setName(tag.name);
    setColor(tag.color && tag.color !== 'none' ? tag.color : null);
    setParentId(tag.parentId ?? '');
    setDescription(tag.description ?? '');
    setError(null);
  }, [tag]);

  if (!tag) return null;

  const parents = parentCandidates(tags, tag.id).sort((a, b) => a.name.localeCompare(b.name));

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) {
      setError('A tag needs a name');
      return;
    }
    setSaving(true);
    try {
      await updateTag(tag.id, { name, color, parentId: parentId || null, description });
      onClose();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className={dialogStyles.overlay}
      onClick={onClose}
      onKeyDown={(e) => e.key === 'Escape' && onClose()}
    >
      <div
        className={clsx(dialogStyles.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="edit-tag-title"
      >
        <div className={dialogStyles.header}>
          <h3 id="edit-tag-title" className={clsx(dialogStyles.title, styles.title)}>
            Edit tag
          </h3>
          <button className={dialogStyles.closeBtn} onClick={onClose} title="Close">
            <IconX size={14} />
          </button>
        </div>

        <form className={clsx(dialogStyles.content, styles.form)} onSubmit={handleSubmit}>
          <label className={styles.label} htmlFor="edit-tag-name">
            Name
          </label>
          <input
            id="edit-tag-name"
            className={styles.input}
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setError(null);
            }}
            autoFocus
            autoComplete="off"
            spellCheck={false}
          />

          <span className={styles.label}>Color</span>
          <div className={styles.colorRow}>
            <button
              type="button"
              className={clsx(styles.colorNone, color === null && styles.colorNoneActive)}
              onClick={() => setColor(null)}
              title="No color"
            >
              <IconX size={10} />
            </button>
            {COLOR_LABELS.map((label) => (
              <button
                type="button"
                key={label}
                className={clsx(styles.colorDot, color === label && styles.colorDotActive)}
                style={{ background: `var(--swatch-${label})` }}
                title={label}
                onClick={() => setColor(label)}
              />
            ))}
          </div>

          <label className={styles.label} htmlFor="edit-tag-parent">
            Parent
          </label>
          <select
            id="edit-tag-parent"
            className={clsx(styles.input, styles.select)}
            value={parentId}
            onChange={(e) => setParentId(e.target.value)}
          >
            <option value="">None</option>
            {parents.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>

          <label className={styles.label} htmlFor="edit-tag-hint">
            Hint
          </label>
          <textarea
            id="edit-tag-hint"
            className={clsx(styles.input, styles.textarea)}
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="What this tag means, for auto-tagging"
            rows={3}
          />

          {error && (
            <p className={styles.error} role="alert">
              {error}
            </p>
          )}

          <div className={dialogStyles.actions}>
            <button
              type="button"
              className={clsx(dialogStyles.btn, dialogStyles.cancelBtn)}
              onClick={onClose}
            >
              Cancel
            </button>
            <button
              type="submit"
              className={clsx(dialogStyles.btn, styles.saveBtn)}
              disabled={saving}
            >
              Save
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
