import { IconChevronDown, IconChevronRight, IconPlus } from '../Icons';
import styles from './Sidebar.module.css';

interface SidebarSectionProps {
  label: string;
  icon: React.ReactNode;
  expanded: boolean;
  onToggle: () => void;
  onAdd?: () => void;
  children: React.ReactNode;
}

export function SidebarSection({
  label,
  icon,
  expanded,
  onToggle,
  onAdd,
  children,
}: SidebarSectionProps) {
  return (
    <div className={styles.section}>
      <div className={styles.sectionHeader}>
        <button className={styles.sectionToggle} onClick={onToggle}>
          {expanded ? <IconChevronDown size={12} /> : <IconChevronRight size={12} />}
          {icon}
          <span className={styles.sectionLabel}>{label}</span>
        </button>
        {onAdd && (
          <button
            className={styles.addBtn}
            onClick={onAdd}
            title={`Add ${label}`}
            id={`add-${label.toLowerCase()}`}
          >
            <IconPlus size={12} />
          </button>
        )}
      </div>
      {expanded && <div className={styles.sectionBody}>{children}</div>}
    </div>
  );
}
