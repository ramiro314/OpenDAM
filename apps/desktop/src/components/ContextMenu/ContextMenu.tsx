import { useEffect, useLayoutEffect, useRef, type ReactNode } from 'react';
import clsx from 'clsx';
import { fitInViewport } from '../../utils/contextMenu';
import styles from './ContextMenu.module.css';

export interface ContextMenuItem {
  id?: string;
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  danger?: boolean;
  /** Why the item can't be used: it is disabled and shows this under its label */
  disabledReason?: string | null;
}

interface ContextMenuProps {
  /** Where the right-click happened */
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
}

/** A right-click menu at the cursor. Choosing an item, clicking outside or Escape closes it. */
export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);

  // Opened at the cursor, then moved back inside the window once its size is known.
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!el) return;
    const { width, height } = el.getBoundingClientRect();
    const { left, top } = fitInViewport(x, y, width, height, window.innerWidth, window.innerHeight);
    el.style.left = `${left}px`;
    el.style.top = `${top}px`;
  }, [x, y]);

  // On the window, not the menu: with every item disabled nothing in the menu can take focus.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  const firstEnabled = items.findIndex((item) => !item.disabledReason);

  return (
    <div
      className={styles.backdrop}
      onClick={onClose}
      onContextMenu={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div
        ref={menuRef}
        className={styles.menu}
        style={{ left: x, top: y }}
        role="menu"
        onClick={(e) => e.stopPropagation()}
      >
        {items.map((item, i) => (
          <button
            key={item.label}
            id={item.id}
            className={clsx(styles.item, item.danger && styles.itemDanger)}
            role="menuitem"
            autoFocus={i === firstEnabled}
            disabled={Boolean(item.disabledReason)}
            onClick={() => {
              onClose();
              item.onSelect();
            }}
          >
            {item.icon}
            <span className={styles.itemText}>
              <span>{item.label}</span>
              {item.disabledReason && (
                <span className={styles.itemReason}>{item.disabledReason}</span>
              )}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}
