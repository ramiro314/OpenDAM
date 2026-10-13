import { useState } from 'react';
import clsx from 'clsx';
import { useUIStore, useAssetStore } from '../../store';
import type { ColorLabel, FilterSpec } from '../../types';
import {
  IconChevronDown,
  IconChevronRight,
  IconInbox,
  IconFilter,
  IconHeartFilled,
  IconStar,
  IconLayers,
  IconModel3D,
  IconImage,
  IconVideo,
  IconEye,
} from '../Icons';
import { SidebarSection } from './SidebarSection';
import { datePresets, isFilterEmpty } from './sidebarHelpers';
import styles from './Sidebar.module.css';

const SIDEBAR_COLOR_LABELS: { label: ColorLabel; name: string; hex: string }[] = [
  { label: 'red', name: 'Red', hex: '#ef4444' },
  { label: 'orange', name: 'Orange', hex: '#f97316' },
  { label: 'yellow', name: 'Yellow', hex: '#eab308' },
  { label: 'green', name: 'Green', hex: '#22c55e' },
  { label: 'teal', name: 'Teal', hex: '#14b8a6' },
  { label: 'blue', name: 'Blue', hex: '#3b82f6' },
  { label: 'purple', name: 'Purple', hex: '#a855f7' },
  { label: 'pink', name: 'Pink', hex: '#ec4899' },
];

const KIND_OPTIONS = [
  { kind: '3d_model' as const, label: '3D Models', icon: <IconModel3D size={13} /> },
  { kind: 'material' as const, label: 'Materials', icon: <IconLayers size={13} /> },
  { kind: 'texture' as const, label: 'Textures', icon: <IconImage size={13} /> },
  { kind: 'hdri' as const, label: 'HDRI', icon: <IconImage size={13} /> },
  { kind: 'image' as const, label: 'Images', icon: <IconImage size={13} /> },
  { kind: 'video' as const, label: 'Video', icon: <IconVideo size={13} /> },
];

const RATING_OPTIONS = [
  { label: '5 Stars', min: 5, max: 5 },
  { label: '4+ Stars', min: 4, max: 5 },
  { label: '3+ Stars', min: 3, max: 5 },
  { label: '1+ Stars', min: 1, max: 5 },
];

const SIZE_OPTIONS = [
  { label: '< 5 MB', min: undefined, max: 5 * 1024 * 1024 },
  { label: '5 MB – 50 MB', min: 5 * 1024 * 1024, max: 50 * 1024 * 1024 },
  { label: '> 50 MB', min: 50 * 1024 * 1024, max: undefined },
];

const PREVIEW_OPTIONS = [
  { label: 'Has Preview', status: 'done' as const },
  { label: 'Pending / Generating', status: 'pending' as const },
  { label: 'Needs Preview', status: 'none' as const },
];

interface FiltersSectionProps {
  clearSmartFilter: () => void;
}

export function FiltersSection({ clearSmartFilter }: FiltersSectionProps) {
  const setActiveCollection = useUIStore((s) => s.setActiveCollection);
  const setFilter = useAssetStore((s) => s.setFilter);
  const resetFilter = useAssetStore((s) => s.resetFilter);
  const clearFilter = useAssetStore((s) => s.clearFilter);
  const currentFilter = useAssetStore((s) => s.query.filter);
  const search = useAssetStore((s) => s.search);

  const [expanded, setExpanded] = useState(true);
  const [subExpanded, setSubExpanded] = useState({
    types: true,
    ratings: true,
    colors: true,
    dateAdded: false,
    fileSize: false,
    previewStatus: false,
  });

  const toggleSub = (key: keyof typeof subExpanded) =>
    setSubExpanded((prev) => ({ ...prev, [key]: !prev[key] }));

  // Picking a filter leaves any smart filter or collection view, then turns the filter on or off
  const toggleFilter = (key: keyof FilterSpec, isActive: boolean, patch: Partial<FilterSpec>) => {
    clearSmartFilter();
    setActiveCollection(null);
    if (isActive) {
      clearFilter(key);
    } else {
      setFilter(patch);
    }
    search();
  };

  const clearSub = (key: keyof FilterSpec) => {
    clearFilter(key);
    search();
  };

  const isFavActive = Boolean(currentFilter.isFavorite);
  const isUnorgActive = Boolean(currentFilter.unorganized);
  const hasKinds = Boolean(currentFilter.kinds && currentFilter.kinds.length > 0);
  const hasColors = Boolean(currentFilter.colorLabels && currentFilter.colorLabels.length > 0);

  return (
    <SidebarSection
      label="Filters"
      icon={<IconFilter size={14} />}
      expanded={expanded}
      onToggle={() => setExpanded((e) => !e)}
    >
      <button
        type="button"
        id="sidebar-filter-all"
        className={clsx(styles.item, isFilterEmpty(currentFilter) && styles.active)}
        onClick={() => {
          clearSmartFilter();
          setActiveCollection(null);
          resetFilter();
          search();
        }}
      >
        <span className={clsx(styles.itemName, 'truncate')}>All assets</span>
      </button>

      <FilterOption
        id="sidebar-filter-favorites"
        label="Favorites"
        icon={
          <span className={styles.favoriteIcon}>
            <IconHeartFilled size={13} />
          </span>
        }
        active={isFavActive}
        activeClassName={styles.favoriteActive}
        onClick={() => toggleFilter('isFavorite', isFavActive, { isFavorite: true })}
      />

      <FilterOption
        id="sidebar-filter-unorganized"
        label="Unorganized"
        icon={<IconInbox size={13} />}
        active={isUnorgActive}
        onClick={() => toggleFilter('unorganized', isUnorgActive, { unorganized: true })}
      />

      <FilterSubGroup
        title="Types"
        expanded={subExpanded.types}
        onToggle={() => toggleSub('types')}
        active={hasKinds}
        onClear={() => clearSub('kinds')}
      >
        {KIND_OPTIONS.map(({ kind, label, icon }) => {
          const isActive = Boolean(currentFilter.kinds?.includes(kind));
          return (
            <FilterOption
              key={kind}
              id={`sidebar-filter-type-${kind}`}
              label={label}
              icon={icon}
              active={isActive}
              onClick={() => toggleFilter('kinds', isActive, { kinds: [kind] })}
            />
          );
        })}
      </FilterSubGroup>

      <FilterSubGroup
        title="Ratings"
        expanded={subExpanded.ratings}
        onToggle={() => toggleSub('ratings')}
        active={Boolean(currentFilter.rating)}
        onClear={() => clearSub('rating')}
      >
        {RATING_OPTIONS.map(({ label, min, max }) => {
          const isActive = currentFilter.rating?.min === min && currentFilter.rating?.max === max;
          return (
            <FilterOption
              key={label}
              id={`sidebar-filter-rating-${min}`}
              label={label}
              icon={<IconStar size={12} className={styles.starActive} />}
              active={isActive}
              onClick={() => toggleFilter('rating', isActive, { rating: { min, max } })}
            />
          );
        })}
      </FilterSubGroup>

      <FilterSubGroup
        title="Color Labels"
        expanded={subExpanded.colors}
        onToggle={() => toggleSub('colors')}
        active={hasColors}
        onClear={() => clearSub('colorLabels')}
        bodyClassName={styles.sidebarColorRow}
      >
        {SIDEBAR_COLOR_LABELS.map(({ label, name, hex }) => {
          const isActive = Boolean(currentFilter.colorLabels?.includes(label));
          return (
            <button
              key={label}
              type="button"
              id={`sidebar-filter-color-${label}`}
              className={clsx(styles.sidebarColorDot, isActive && styles.sidebarColorActive)}
              style={{ backgroundColor: hex }}
              onClick={() => toggleFilter('colorLabels', isActive, { colorLabels: [label] })}
              title={name}
            />
          );
        })}
      </FilterSubGroup>

      <FilterSubGroup
        title="Date Added"
        expanded={subExpanded.dateAdded}
        onToggle={() => toggleSub('dateAdded')}
        active={Boolean(currentFilter.dateRange)}
        onClear={() => clearSub('dateRange')}
      >
        {datePresets(currentFilter.dateRange?.from).map(({ label, from, active }) => (
          <FilterOption
            key={label}
            label={label}
            active={active}
            onClick={() => toggleFilter('dateRange', active, { dateRange: { from } })}
          />
        ))}
      </FilterSubGroup>

      <FilterSubGroup
        title="File Size"
        expanded={subExpanded.fileSize}
        onToggle={() => toggleSub('fileSize')}
        active={Boolean(currentFilter.sizeRange)}
        onClear={() => clearSub('sizeRange')}
      >
        {SIZE_OPTIONS.map(({ label, min, max }) => {
          const isActive =
            currentFilter.sizeRange?.min === min && currentFilter.sizeRange?.max === max;
          return (
            <FilterOption
              key={label}
              label={label}
              active={isActive}
              onClick={() => toggleFilter('sizeRange', isActive, { sizeRange: { min, max } })}
            />
          );
        })}
      </FilterSubGroup>

      <FilterSubGroup
        title="Preview Status"
        expanded={subExpanded.previewStatus}
        onToggle={() => toggleSub('previewStatus')}
        active={Boolean(currentFilter.previewStatus)}
        onClear={() => clearSub('previewStatus')}
      >
        {PREVIEW_OPTIONS.map(({ label, status }) => {
          const isActive = currentFilter.previewStatus === status;
          return (
            <FilterOption
              key={label}
              label={label}
              icon={<IconEye size={13} />}
              active={isActive}
              onClick={() => toggleFilter('previewStatus', isActive, { previewStatus: status })}
            />
          );
        })}
      </FilterSubGroup>
    </SidebarSection>
  );
}

interface FilterSubGroupProps {
  title: string;
  expanded: boolean;
  onToggle: () => void;
  active: boolean;
  onClear: () => void;
  bodyClassName?: string;
  children: React.ReactNode;
}

/** A collapsible group of filter options, with a Clear link while one of them is set. */
function FilterSubGroup({
  title,
  expanded,
  onToggle,
  active,
  onClear,
  bodyClassName = styles.subGroupBody,
  children,
}: FilterSubGroupProps) {
  return (
    <div className={styles.subGroup}>
      <button type="button" className={styles.subGroupHeader} onClick={onToggle}>
        <span className={styles.subGroupTitle}>
          {expanded ? <IconChevronDown size={10} /> : <IconChevronRight size={10} />}
          <span>{title}</span>
          {active && <span className={styles.filterActiveChip}>●</span>}
        </span>
        {active && (
          <span
            role="button"
            tabIndex={0}
            className={styles.clearSubFilterBtn}
            onClick={(e) => {
              e.stopPropagation();
              onClear();
            }}
          >
            Clear
          </span>
        )}
      </button>
      {expanded && <div className={bodyClassName}>{children}</div>}
    </div>
  );
}

interface FilterOptionProps {
  id?: string;
  label: string;
  icon?: React.ReactNode;
  active: boolean;
  activeClassName?: string;
  onClick: () => void;
}

function FilterOption({
  id,
  label,
  icon,
  active,
  activeClassName = styles.active,
  onClick,
}: FilterOptionProps) {
  return (
    <button
      type="button"
      id={id}
      className={clsx(styles.item, active && activeClassName)}
      onClick={onClick}
    >
      {icon}
      <span className={clsx(styles.itemName, 'truncate')}>{label}</span>
      {active && <span className={styles.filterActiveChip}>●</span>}
    </button>
  );
}
