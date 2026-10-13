import clsx from 'clsx';
import { IconCheck, IconX } from '../Icons';
import styles from './Sidebar.module.css';

interface InlineCreateFormProps {
  placeholder: string;
  saveTitle: string;
  value: string;
  error: string | null;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onCancel: () => void;
  className?: string;
}

/** The name input with save and cancel buttons used to create a collection or tag in place. */
export function InlineCreateForm({
  placeholder,
  saveTitle,
  value,
  error,
  onChange,
  onSubmit,
  onCancel,
  className,
}: InlineCreateFormProps) {
  return (
    <div className={clsx(styles.createFormWrap, className)}>
      <form
        className={styles.createForm}
        onSubmit={(e) => {
          e.preventDefault();
          onSubmit();
        }}
      >
        <input
          type="text"
          autoFocus
          placeholder={placeholder}
          className={styles.createInput}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') onCancel();
          }}
        />
        <button
          type="submit"
          className={clsx(styles.createBtn, styles.createBtnConfirm)}
          title={saveTitle}
        >
          <IconCheck size={12} />
        </button>
        <button
          type="button"
          className={clsx(styles.createBtn, styles.createBtnCancel)}
          onClick={onCancel}
          title="Cancel"
        >
          <IconX size={12} />
        </button>
      </form>
      {error && <span className={styles.formError}>{error}</span>}
    </div>
  );
}
