import { useState } from 'react';
import { LibrariesSection } from './LibrariesSection';
import { CollectionsSection } from './CollectionsSection';
import { TagsSection } from './TagsSection';
import { SmartFiltersSection } from './SmartFiltersSection';
import { FiltersSection } from './FiltersSection';
import { ImportsSection } from './ImportsSection';
import styles from './Sidebar.module.css';

export function Sidebar() {
  // The active smart filter lives here because picking anything in another section turns it off
  const [activeSmartFilterId, setActiveSmartFilterId] = useState<string | null>(null);
  const clearSmartFilter = () => setActiveSmartFilterId(null);

  return (
    <aside className={styles.sidebar} id="sidebar">
      <LibrariesSection clearSmartFilter={clearSmartFilter} />
      <ImportsSection />
      <CollectionsSection clearSmartFilter={clearSmartFilter} />
      <TagsSection clearSmartFilter={clearSmartFilter} />
      <SmartFiltersSection activeId={activeSmartFilterId} setActiveId={setActiveSmartFilterId} />
      <FiltersSection clearSmartFilter={clearSmartFilter} />
    </aside>
  );
}
