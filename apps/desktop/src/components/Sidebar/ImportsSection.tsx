import { useState } from 'react';
import { isLive, useImportStore } from '../../store/imports';
import { importPack } from '../Imports/ImportsPanel';
import { IconInbox, IconPlus } from '../Icons';
import { SidebarSection } from './SidebarSection';
import styles from './Sidebar.module.css';

/** The Imports view's sidebar entry: the view itself, and Import pack */
export function ImportsSection() {
  const [expanded, setExpanded] = useState(true);
  const setPanelOpen = useImportStore((s) => s.setPanelOpen);
  const imports = useImportStore((s) => Object.values(s.imports).flat().length);
  const live = useImportStore((s) => s.runs.filter(isLive).length);

  return (
    <SidebarSection
      label="Imports"
      icon={<IconInbox size={14} />}
      expanded={expanded}
      onToggle={() => setExpanded(!expanded)}
    >
      <button className={styles.item} onClick={() => setPanelOpen(true)} id="open-imports">
        <IconInbox size={14} />
        <span className={styles.itemName}>All imports</span>
        {live > 0 ? (
          <span className={styles.badge} title="Import runs queued or running">
            {live} running
          </span>
        ) : (
          imports > 0 && <span className={styles.badge}>{imports}</span>
        )}
      </button>
      <button
        className={styles.item}
        onClick={() => {
          setPanelOpen(true);
          importPack().catch((e) => useImportStore.setState({ error: String(e) }));
        }}
        title="Bring a .unitypackage or .zip into a library as one or more packs. The original is only read."
      >
        <IconPlus size={14} />
        <span className={styles.itemName}>Import pack…</span>
      </button>
    </SidebarSection>
  );
}
