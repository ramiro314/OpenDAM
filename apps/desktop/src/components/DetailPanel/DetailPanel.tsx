import { useCallback, useState, useMemo, useEffect } from 'react';
import clsx from 'clsx';
import {
  useUIStore,
  useAssetStore,
  useTagStore,
  useCollectionStore,
  useGroupDetailStore,
  usePackDetailStore,
  usePacksStore,
  useFolderActionStore,
  lookupAsset,
  packRef,
  shownMember,
} from '../../store';
import { convertFileSrc } from '@tauri-apps/api/core';
import type {
  Asset,
  AssetKind,
  ColorLabel,
  Collection,
  Cover,
  GroupMember,
  LicenseKind,
  PackDetail,
} from '../../types';
import {
  IconStar,
  IconX,
  IconExternal,
  IconEye,
  IconInfo,
  IconCopy,
  IconCheck,
  IconRefresh,
  IconEdit,
  IconTrash,
  IconHeart,
  IconHeartFilled,
  IconChevronRight,
  IconPackage,
} from '../Icons';
import { ConfirmDeleteDialog } from '../ConfirmDeleteDialog';
import { api } from '../../api';
import { imageOf } from '../../utils/media';
import { formatSize, itemFileCount } from '../../utils/format';
import { useImageScaling } from '../../hooks/useImageScaling';
import { useBrowseView } from '../../hooks/useBrowseView';
import { panelPackTarget } from '../../utils/packsView';
import { detailsEdit, packForm, sourceUrlProblem, type PackForm } from '../../utils/packEdits';
import { KIND_LABEL } from '../../utils/scrubKey';
import { ThreeViewer } from './ThreeViewer';
import { SuggestTags } from './SuggestTags';
import styles from './DetailPanel.module.css';

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

export function DetailPanel() {
  const focusedAssetId = useUIStore((s) => s.focusedAssetId);
  const asset = useAssetStore((s) =>
    focusedAssetId ? (lookupAsset(s, focusedAssetId) ?? null) : null
  );
  const patchAsset = useAssetStore((s) => s.patchAsset);
  const deleteAsset = useAssetStore((s) => s.deleteAsset);
  const renameAsset = useAssetStore((s) => s.renameAsset);
  const tags = useTagStore((s) => s.tags);
  const collections = useCollectionStore((s) => s.collections);
  const addToCollection = useCollectionStore((s) => s.addToCollection);
  const removeFromCollection = useCollectionStore((s) => s.removeFromCollection);
  const [tab, setTab] = useState<'info' | 'tags' | 'collections' | 'preview'>('info');

  const [isEditingName, setIsEditingName] = useState(false);
  const [editingName, setEditingName] = useState('');
  const [panelError, setPanelError] = useState<string | null>(null);
  const [isDeleteDialogOpen, setIsDeleteDialogOpen] = useState(false);

  // With no asset shown, the panel shows a pack (plan C18): the Packs view's selected card, or
  // the pack holding the selected folder
  const { view } = useBrowseView();
  const query = useAssetStore((s) => s.query);
  const selectedCard = usePacksStore((s) => s.selected);
  const pack = usePackDetailStore((s) => s.detail);
  const loadPack = usePackDetailStore((s) => s.load);
  const packTarget = panelPackTarget(view, selectedCard ? packRef(selectedCard) : null, query);
  useEffect(() => {
    void loadPack(packTarget);
    // The target's library and path stand for it
  }, [packTarget?.libraryId, packTarget?.path]); // eslint-disable-line react-hooks/exhaustive-deps

  // A group card lists its files; the panel shows one of them in place of the group on a click,
  // while the grid keeps the card selected (plan C19)
  const groupId = useGroupDetailStore((s) => s.groupId);
  const members = useGroupDetailStore((s) => s.members);
  const member = useGroupDetailStore(shownMember);
  const loadGroup = useGroupDetailStore((s) => s.load);
  const showMember = useGroupDetailStore((s) => s.showMember);
  const backToGroup = useGroupDetailStore((s) => s.backToGroup);
  // Reloaded whenever the cached card changes, so a refresh after a rename or delete shows here
  useEffect(() => {
    loadGroup(asset?.groupFormats ? asset.id : null);
  }, [asset, loadGroup]);

  useEffect(() => {
    setIsEditingName(false);
    setPanelError(null);
    if (asset) {
      setEditingName(asset.fileName);
    }
  }, [asset?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const setRating = useCallback(
    (rating: number) => asset && patchAsset(asset.id, { rating }),
    [asset, patchAsset]
  );

  const setColorLabel = useCallback(
    (label: ColorLabel) =>
      asset && patchAsset(asset.id, { colorLabel: label === asset.colorLabel ? 'none' : label }),
    [asset, patchAsset]
  );

  const toggleFavorite = useCallback(
    () => asset && patchAsset(asset.id, { isFavorite: !asset.isFavorite }),
    [asset, patchAsset]
  );

  const toggleTag = useCallback(
    (tagId: string) => {
      if (!asset) return;
      const next = asset.tags.includes(tagId)
        ? asset.tags.filter((t) => t !== tagId)
        : [...asset.tags, tagId];
      patchAsset(asset.id, { tags: next });
    },
    [asset, patchAsset]
  );

  const setPreviewStatus = useAssetStore((s) => s.setPreviewStatus);
  const handleGeneratePreview = useCallback(async () => {
    if (!asset) return;
    setPreviewStatus(asset.id, 'pending');
    try {
      await api.requestPreview([asset.id]);
    } catch (err) {
      console.error('Failed to request preview:', err);
      setPreviewStatus(asset.id, 'error');
    }
  }, [asset, setPreviewStatus]);

  const handleStartRename = () => {
    if (!asset) return;
    setIsEditingName(true);
    setEditingName(asset.fileName);
    setPanelError(null);
  };

  const handleSaveRename = async () => {
    if (!asset) return;
    const trimmed = editingName.trim();
    if (!trimmed || trimmed === asset.fileName) {
      setIsEditingName(false);
      setPanelError(null);
      return;
    }
    try {
      await renameAsset(asset.id, trimmed);
      setIsEditingName(false);
      setPanelError(null);
    } catch (err: any) {
      setPanelError(err?.message || String(err));
    }
  };

  const handleConfirmDelete = async () => {
    if (!asset) return;
    setIsDeleteDialogOpen(false);
    try {
      await deleteAsset(asset.id, true);
    } catch (err: any) {
      setPanelError(err?.message || String(err));
    }
  };

  if (pack && (!asset || view === 'packs')) return <PackPanel pack={pack} />;
  if (!asset) {
    return (
      <aside className={styles.panel} id="detail-panel">
        <div className={styles.empty}>
          <IconInfo size={32} />
          <p>
            {view === 'packs' ? 'Select a pack to see details' : 'Select an asset to see details'}
          </p>
        </div>
      </aside>
    );
  }

  const groupMembers = groupId === asset.id && members.length > 1 ? members : [];
  if (member && groupId === asset.id) {
    return (
      <aside className={styles.panel} id="detail-panel" data-group-member-view={member.id}>
        <div className={styles.memberBar}>
          <button
            id="detail-back-to-group"
            className={styles.backBtn}
            onClick={backToGroup}
            title={`Back to ${asset.fileName}`}
          >
            <IconChevronRight size={12} className={styles.backIcon} />
            <span>Back to group</span>
          </button>
        </div>
        <div className={styles.thumbArea}>
          <AssetPreview asset={member} />
        </div>
        <div className={styles.header}>
          <p className={clsx(styles.fileName, 'truncate')} title={member.fileName}>
            {member.fileName}
          </p>
          <div className={styles.headerActions}>
            <FileActions filePath={member.filePath} idPrefix="detail-member" />
          </div>
        </div>
        <div className={styles.tabContent}>
          <div className={styles.infoGrid}>
            <FileInfo asset={member} />
          </div>
        </div>
      </aside>
    );
  }

  const isPending = asset.previewStatus === 'pending' || asset.previewStatus === 'generating';

  return (
    <aside className={styles.panel} id="detail-panel">
      {/* Preview thumbnail */}
      <div className={styles.thumbArea}>
        <AssetPreview asset={asset} onGeneratePreview={handleGeneratePreview} />
      </div>

      {/* File name + actions */}
      <div className={styles.header}>
        {isEditingName ? (
          <form
            className={styles.nameEditForm}
            onSubmit={(e) => {
              e.preventDefault();
              handleSaveRename();
            }}
          >
            <input
              type="text"
              autoFocus
              className={styles.nameInput}
              value={editingName}
              onChange={(e) => {
                setEditingName(e.target.value);
                if (panelError) setPanelError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === 'Escape') {
                  setIsEditingName(false);
                  setPanelError(null);
                }
              }}
            />
            <button
              type="submit"
              className={clsx(styles.nameActionBtn, styles.nameActionBtnConfirm)}
              title="Save name"
            >
              <IconCheck size={13} />
            </button>
            <button
              type="button"
              className={clsx(styles.nameActionBtn, styles.nameActionBtnCancel)}
              onClick={() => {
                setIsEditingName(false);
                setPanelError(null);
              }}
              title="Cancel"
            >
              <IconX size={13} />
            </button>
          </form>
        ) : (
          <p
            className={clsx(styles.fileName, 'truncate')}
            title="Double-click or click edit icon to rename"
            onDoubleClick={handleStartRename}
            style={{ cursor: 'pointer' }}
          >
            {asset.fileName}
          </p>
        )}

        <div className={styles.headerActions}>
          <button
            id="detail-favorite"
            className={clsx(styles.actionBtn, asset.isFavorite && styles.actionBtnFavorite)}
            title={asset.isFavorite ? 'Remove from favorites' : 'Add to favorites'}
            onClick={toggleFavorite}
          >
            {asset.isFavorite ? (
              <IconHeartFilled size={14} className={styles.favoriteHeartFilled} />
            ) : (
              <IconHeart size={14} />
            )}
          </button>
          <button
            id="detail-rename"
            className={styles.actionBtn}
            title="Rename file (F2)"
            onClick={handleStartRename}
          >
            <IconEdit size={14} />
          </button>
          <button
            id="detail-gen-preview-header"
            className={clsx(styles.actionBtn, isPending && styles.actionBtnLoading)}
            title={isPending ? 'Generating preview…' : 'Generate/Regenerate preview'}
            onClick={handleGeneratePreview}
            disabled={isPending}
          >
            <IconRefresh size={14} className={clsx(isPending && 'animate-spin')} />
          </button>
          <FileActions filePath={asset.filePath} idPrefix="detail" />
          <button
            id="detail-delete"
            className={clsx(styles.actionBtn, styles.actionBtnDanger)}
            title="Delete file (Move to Recycle Bin)"
            onClick={() => setIsDeleteDialogOpen(true)}
          >
            <IconTrash size={14} />
          </button>
        </div>
      </div>

      {panelError && (
        <div
          style={{
            color: '#ef4444',
            fontSize: '11px',
            padding: '2px 16px',
            background: 'rgba(239, 68, 68, 0.1)',
          }}
        >
          {panelError}
        </div>
      )}

      {groupMembers.length > 0 && <GroupFiles members={groupMembers} onShow={showMember} />}

      {/* Tabs */}
      <div className={styles.tabs}>
        {(['info', 'tags', 'collections', 'preview'] as const).map((t) => (
          <button
            key={t}
            id={`detail-tab-${t}`}
            className={clsx(styles.tab, tab === t && styles.tabActive)}
            onClick={() => setTab(t)}
          >
            {t.charAt(0).toUpperCase() + t.slice(1)}
          </button>
        ))}
      </div>

      <div className={styles.tabContent}>
        {tab === 'info' && (
          <InfoTab
            asset={asset}
            onRatingChange={setRating}
            onColorLabelChange={setColorLabel}
            onFavoriteChange={toggleFavorite}
          />
        )}
        {tab === 'tags' && <TagsTab asset={asset} tags={tags} onToggle={toggleTag} />}
        {tab === 'collections' && (
          <CollectionsTab
            asset={asset}
            collections={collections}
            onAdd={async (colId) => {
              await addToCollection(colId, [asset.id]);
              useCollectionStore.getState().fetchCollections();
            }}
            onRemove={async (colId) => {
              await removeFromCollection(colId, [asset.id]);
              useCollectionStore.getState().fetchCollections();
            }}
          />
        )}
        {tab === 'preview' && (
          <PreviewTab asset={asset} onGeneratePreview={handleGeneratePreview} />
        )}
      </div>

      <ConfirmDeleteDialog
        isOpen={isDeleteDialogOpen}
        itemType="asset"
        itemName={asset.fileName}
        fileCount={itemFileCount(asset)}
        onConfirm={handleConfirmDelete}
        onCancel={() => setIsDeleteDialogOpen(false)}
      />
    </aside>
  );
}

// ── Pack ──────────────────────────────────────────────────────────────────
const LICENSE_LABEL: Record<LicenseKind, string> = {
  CC0: 'CC0',
  'CC-BY': 'CC-BY',
  'CC-BY-SA': 'CC-BY-SA',
  MIT: 'MIT',
  'Apache-2.0': 'Apache-2.0',
  OFL: 'OFL',
  unknown: 'Unknown',
};

/** The image that stands for a cover, as `imageOf` picks it for an asset. */
const coverImage = (cover: Cover) =>
  imageOf({ ...cover, thumbnailPath: cover.thumbnailPath ?? undefined });

/**
 * A pack's details (plan C18): its cover, or the gallery image clicked, its name, its gallery of
 * `Preview*` and `Sample*` images, its Kenney header's version and author, its license with "Open
 * license", its source URL with "Open", and its files by kind with their total size. The edit
 * button opens a form for its name, license and source URL. Each save reruns the structure pass,
 * whose `library:updated` reloads the pack here.
 */
function PackPanel({ pack }: { pack: PackDetail }) {
  const scaling = useImageScaling('contain');
  const [shown, setShown] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    setShown(null);
    setEditing(false);
  }, [pack.libraryId, pack.path]);
  const image = pack.gallery.find((g) => g.filePath === shown) ?? pack.cover;
  const src = image ? coverImage(image) : undefined;
  const kinds = (Object.entries(pack.kinds) as [AssetKind, number][]).sort(
    ([a, m], [b, n]) => n - m || a.localeCompare(b)
  );
  const files = kinds.reduce((sum, [, n]) => sum + n, 0);
  const { header, license, edits } = pack;
  const licenseName =
    edits.license ?? (license ? (LICENSE_LABEL[license.kind] ?? license.kind) : null);
  const resetCover = async () => {
    try {
      await api.setPackOverride(pack.libraryId, pack.path, { kind: 'cover', assetId: null });
    } catch (err) {
      console.error('Failed to reset the pack cover:', err);
      useFolderActionStore
        .getState()
        .setFailure(`Resetting the cover of ${pack.name} failed: ${String(err)}`);
    }
  };

  return (
    <aside
      className={styles.panel}
      id="detail-panel"
      data-pack-detail={pack.path}
      data-pack-library={pack.libraryId}
    >
      <div className={styles.thumbArea}>
        {src ? (
          <img
            src={convertFileSrc(src)}
            alt={pack.name}
            className={styles.thumbImg}
            ref={scaling}
          />
        ) : (
          <div className={styles.thumbPlaceholder}>
            <IconPackage size={40} className={styles.thumbPackIcon} />
          </div>
        )}
      </div>

      <div className={styles.header}>
        <p className={clsx(styles.fileName, styles.packName)} title={pack.path || pack.name}>
          <IconPackage size={14} className={styles.packIcon} />
          <span id="detail-pack-name" className="truncate">
            {pack.name}
          </span>
        </p>
        <div className={styles.headerActions}>
          {edits.coverPath && (
            <button
              id="detail-pack-reset-cover"
              className={styles.actionBtn}
              title="Use the detected cover"
              onClick={resetCover}
            >
              <IconRefresh size={14} />
            </button>
          )}
          <button
            id="detail-pack-edit"
            className={clsx(styles.actionBtn, editing && styles.actionBtnActive)}
            title="Edit the pack's name, license and source URL"
            aria-pressed={editing}
            onClick={() => setEditing((e) => !e)}
          >
            <IconEdit size={14} />
          </button>
        </div>
      </div>

      {editing && <PackEditForm pack={pack} onDone={() => setEditing(false)} />}

      {pack.gallery.length > 0 && (
        <section className={styles.groupFiles} id="detail-pack-gallery">
          <p className={styles.groupFilesTitle}>Gallery ({pack.gallery.length})</p>
          <div className={styles.packGallery}>
            {pack.gallery.map((g) => {
              const thumb = coverImage(g);
              const name = g.filePath.slice(g.filePath.lastIndexOf('/') + 1);
              return (
                <button
                  key={g.filePath}
                  data-gallery-image={g.filePath}
                  className={clsx(
                    styles.packGalleryItem,
                    image?.filePath === g.filePath && styles.packGalleryShown
                  )}
                  title={name}
                  onClick={() => setShown(g.filePath)}
                >
                  {thumb ? (
                    <img src={convertFileSrc(thumb)} alt={name} loading="lazy" />
                  ) : (
                    <span className={styles.groupFileExt}>{g.extension.toUpperCase()}</span>
                  )}
                </button>
              );
            })}
          </div>
        </section>
      )}

      <div className={styles.tabContent}>
        <div className={styles.infoGrid}>
          {header?.version && (
            <>
              <label className={styles.infoLabel}>Version</label>
              <span id="detail-pack-version" className={styles.infoValue}>
                {header.version}
              </span>
            </>
          )}
          {header?.author && (
            <>
              <label className={styles.infoLabel}>Author</label>
              <span id="detail-pack-author" className={styles.infoValue}>
                {header.author}
              </span>
            </>
          )}

          <label className={styles.infoLabel}>License</label>
          {license ? (
            <span className={styles.licenseRow}>
              <span id="detail-pack-license" className={styles.infoValue}>
                {licenseName}
              </span>
              <button
                id="detail-open-license"
                className={styles.openLicenseBtn}
                title={license.filePath}
                onClick={async () => {
                  try {
                    await api.openWithDefault(license.filePath);
                  } catch (err) {
                    console.error('Failed to open the license:', err);
                  }
                }}
              >
                <IconExternal size={11} />
                <span>Open license</span>
              </button>
            </span>
          ) : (
            <span id="detail-pack-license" className={styles.infoValue}>
              {licenseName ?? 'No license file'}
            </span>
          )}

          {edits.sourceUrl && (
            <>
              <label className={styles.infoLabel}>Source</label>
              <span className={styles.licenseRow}>
                <span id="detail-pack-source" className={clsx(styles.infoValue, styles.mono)}>
                  {edits.sourceUrl}
                </span>
                <button
                  id="detail-open-source"
                  className={styles.openLicenseBtn}
                  title={edits.sourceUrl}
                  onClick={async () => {
                    try {
                      await api.openUrl(edits.sourceUrl!);
                    } catch (err) {
                      console.error('Failed to open the source URL:', err);
                      useFolderActionStore
                        .getState()
                        .setFailure(`Opening ${edits.sourceUrl} failed: ${String(err)}`);
                    }
                  }}
                >
                  <IconExternal size={11} />
                  <span>Open</span>
                </button>
              </span>
            </>
          )}

          <label className={styles.infoLabel}>Items</label>
          <span className={styles.infoValue}>{pack.itemCount.toLocaleString()}</span>

          <label className={styles.infoLabel}>Files</label>
          <ul id="detail-pack-kinds" className={clsx(styles.infoValue, styles.packKinds)}>
            {kinds.map(([kind, n]) => (
              <li key={kind} data-kind={kind}>
                <span>{KIND_LABEL[kind] ?? kind}</span>
                <span className={styles.mono}>{n.toLocaleString()}</span>
              </li>
            ))}
            {kinds.length > 1 && (
              <li>
                <span>Total</span>
                <span className={styles.mono}>{files.toLocaleString()}</span>
              </li>
            )}
          </ul>

          <label className={styles.infoLabel}>Size</label>
          <span id="detail-pack-size" className={clsx(styles.infoValue, styles.mono)}>
            {formatSize(pack.sizeBytes)}
          </span>

          <label className={styles.infoLabel}>Folder</label>
          <span className={clsx(styles.infoValue, styles.mono)}>{pack.path || '/'}</span>
        </div>
      </div>
    </aside>
  );
}

/**
 * The pack's edit form: its name, license and source URL. An empty field goes back to what the
 * structure pass detected.
 */
function PackEditForm({ pack, onDone }: { pack: PackDetail; onDone: () => void }) {
  const [form, setForm] = useState<PackForm>(() => packForm(pack));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const urlProblem = sourceUrlProblem(form.sourceUrl);
  const field = (key: keyof PackForm) => ({
    value: form[key],
    onChange: (e: React.ChangeEvent<HTMLInputElement>) => {
      setForm({ ...form, [key]: e.target.value });
      setError(null);
    },
  });

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (urlProblem) return;
    setSaving(true);
    try {
      await api.setPackOverride(pack.libraryId, pack.path, detailsEdit(form, pack));
      onDone();
    } catch (err) {
      setError(String(err));
      setSaving(false);
    }
  };

  return (
    <form
      id="detail-pack-form"
      className={styles.packForm}
      onSubmit={handleSubmit}
      onKeyDown={(e) => e.key === 'Escape' && onDone()}
    >
      <label className={styles.infoLabel} htmlFor="detail-pack-form-name">
        Name
      </label>
      <input
        id="detail-pack-form-name"
        className={styles.packFormInput}
        placeholder="The detected name"
        autoFocus
        {...field('name')}
      />
      <label className={styles.infoLabel} htmlFor="detail-pack-form-license">
        License
      </label>
      <input
        id="detail-pack-form-license"
        className={styles.packFormInput}
        placeholder={pack.license ? LICENSE_LABEL[pack.license.kind] : 'CC0, CC-BY 4.0...'}
        {...field('license')}
      />
      <label className={styles.infoLabel} htmlFor="detail-pack-form-source">
        Source URL
      </label>
      <input
        id="detail-pack-form-source"
        className={styles.packFormInput}
        type="url"
        placeholder="https://"
        {...field('sourceUrl')}
      />
      {(urlProblem || error) && (
        <p className={styles.packFormError} role="alert">
          {urlProblem ?? error}
        </p>
      )}
      <div className={styles.packFormActions}>
        <span className={styles.packFormHint}>Leave a field empty to use what was detected.</span>
        <button type="button" className={styles.openLicenseBtn} onClick={onDone}>
          Cancel
        </button>
        <button
          type="submit"
          id="detail-pack-form-save"
          className={clsx(styles.openLicenseBtn, styles.packFormSave)}
          disabled={saving || Boolean(urlProblem)}
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </form>
  );
}

// ── Info Tab ──────────────────────────────────────────────────────────────
function InfoTab({
  asset,
  onRatingChange,
  onColorLabelChange,
  onFavoriteChange,
}: {
  asset: Asset;
  onRatingChange: (r: number) => void;
  onColorLabelChange: (l: ColorLabel) => void;
  onFavoriteChange: () => void;
}) {
  const [hoverRating, setHoverRating] = useState(0);

  return (
    <div className={styles.infoGrid}>
      {/* Favorite */}
      <label className={styles.infoLabel}>Favorite</label>
      <div className={styles.favoriteRow}>
        <button
          id="detail-info-favorite"
          type="button"
          className={clsx(styles.infoFavoriteBtn, asset.isFavorite && styles.infoFavoriteActive)}
          onClick={onFavoriteChange}
        >
          {asset.isFavorite ? (
            <>
              <IconHeartFilled size={14} className={styles.favoriteHeartFilled} />
              <span>In Favorites</span>
            </>
          ) : (
            <>
              <IconHeart size={14} />
              <span>Add to Favorites</span>
            </>
          )}
        </button>
      </div>
      {/* Rating */}
      <label className={styles.infoLabel}>Rating</label>
      <div className={styles.starRow}>
        {Array.from({ length: 5 }).map((_, i) => {
          const n = i + 1;
          return (
            <button
              key={n}
              id={`detail-star-${n}`}
              className={clsx(styles.starBtn, (hoverRating || asset.rating) >= n && styles.starOn)}
              onMouseEnter={() => setHoverRating(n)}
              onMouseLeave={() => setHoverRating(0)}
              onClick={() => onRatingChange(asset.rating === n ? 0 : n)}
            >
              <IconStar size={16} />
            </button>
          );
        })}
      </div>

      {/* Color label */}
      <label className={styles.infoLabel}>Label</label>
      <div className={styles.colorRow}>
        <button
          className={clsx(
            styles.colorNoneBtn,
            asset.colorLabel === 'none' && styles.colorNoneActive
          )}
          onClick={() => onColorLabelChange('none')}
          title="No label"
        >
          <IconX size={10} />
        </button>
        {COLOR_LABELS.map((label) => (
          <button
            key={label}
            id={`detail-color-${label}`}
            className={clsx(styles.colorDot, asset.colorLabel === label && styles.colorDotActive)}
            style={{ background: `var(--swatch-${label})` }}
            title={label}
            onClick={() => onColorLabelChange(label)}
          />
        ))}
      </div>

      <FileInfo asset={asset} />
    </div>
  );
}

// ── File info ─────────────────────────────────────────────────────────────
/** A file's own details, as rows of an `infoGrid`: the Info tab's and a group member's. */
function FileInfo({ asset }: { asset: Asset }) {
  return (
    <>
      <label className={styles.infoLabel}>Path</label>
      <span
        className={clsx(styles.infoValue, styles.mono, 'truncate')}
        title={`${asset.filePath} (Click to reveal in file manager)`}
        style={{ cursor: 'pointer' }}
        onClick={async () => {
          try {
            await api.revealInFileManager(asset.filePath);
          } catch (err) {
            console.error('Failed to reveal in file manager:', err);
          }
        }}
      >
        {asset.filePath}
      </span>

      <label className={styles.infoLabel}>Format</label>
      <span className={styles.infoValue}>.{asset.extension.toUpperCase()}</span>

      <label className={styles.infoLabel}>Kind</label>
      <span className={styles.infoValue}>{asset.kind.replace('_', ' ')}</span>

      <label className={styles.infoLabel}>Size</label>
      <span className={clsx(styles.infoValue, styles.mono)}>
        {(asset.sizeBytes / 1024 / 1024).toFixed(2)} MB
      </span>

      <label className={styles.infoLabel}>Modified</label>
      <span className={styles.infoValue}>{new Date(asset.modifiedAt).toLocaleDateString()}</span>

      <label className={styles.infoLabel}>Added</label>
      <span className={styles.infoValue}>{new Date(asset.indexedAt).toLocaleDateString()}</span>

      {/* Extracted metadata */}
      {Object.entries(asset.metadata).length > 0 && (
        <>
          <div className={styles.metaDivider} />
          <label className={clsx(styles.infoLabel, styles.metaHeader)}>Metadata</label>
          <div />
          {Object.entries(asset.metadata).map(([k, v]) => (
            <>
              <label key={`k-${k}`} className={clsx(styles.infoLabel, styles.metaKey)}>
                {k}
              </label>
              <span key={`v-${k}`} className={clsx(styles.infoValue, styles.mono)}>
                {String(v)}
              </span>
            </>
          ))}
        </>
      )}
    </>
  );
}

// ── Tags Tab ──────────────────────────────────────────────────────────────
function TagsTab({
  asset,
  tags,
  onToggle,
}: {
  asset: Asset;
  tags: import('../../types').Tag[];
  onToggle: (id: string) => void;
}) {
  const assigned = tags.filter((t) => asset.tags.includes(t.id));
  const unassigned = tags.filter((t) => !asset.tags.includes(t.id));

  return (
    <div className={styles.tagsSection}>
      {/* Keyed so another asset gets a fresh instance: a late reply for the last one is dropped */}
      <SuggestTags key={asset.id} assetId={asset.id} />

      {tags.length === 0 && (
        <p className={styles.noTags}>No tags yet. Create tags in the sidebar.</p>
      )}

      {assigned.length > 0 && (
        <>
          <p className={styles.tagsGroupLabel}>Assigned</p>
          <div className={styles.tagsGrid}>
            {assigned.map((tag) => (
              <button
                key={tag.id}
                id={`detail-tag-${tag.id}`}
                className={clsx(styles.tagPill, styles.tagActive)}
                onClick={() => onToggle(tag.id)}
                title="Click to remove"
              >
                <span
                  className={styles.tagDot}
                  style={{
                    background: tag.color
                      ? `var(--swatch-${tag.color})`
                      : 'var(--color-text-muted)',
                  }}
                />
                <span className={styles.tagPillName}>{tag.name}</span>
                <IconCheck size={11} className={styles.tagCheckIcon} />
              </button>
            ))}
          </div>
        </>
      )}

      {unassigned.length > 0 && (
        <>
          <p className={styles.tagsGroupLabel}>{assigned.length > 0 ? 'Available' : 'All Tags'}</p>
          <div className={styles.tagsGrid}>
            {unassigned.map((tag) => (
              <button
                key={tag.id}
                id={`detail-tag-${tag.id}`}
                className={styles.tagPill}
                onClick={() => onToggle(tag.id)}
                title="Click to assign"
              >
                <span
                  className={styles.tagDot}
                  style={{
                    background: tag.color
                      ? `var(--swatch-${tag.color})`
                      : 'var(--color-text-muted)',
                  }}
                />
                <span className={styles.tagPillName}>{tag.name}</span>
              </button>
            ))}
          </div>
        </>
      )}
    </div>
  );
}

// ── Collections Tab ───────────────────────────────────────────────────────
function CollectionsTab({
  asset,
  collections,
  onAdd,
  onRemove,
}: {
  asset: Asset;
  collections: Collection[];
  onAdd: (collectionId: string) => void;
  onRemove: (collectionId: string) => void;
}) {
  const regularCollections = useMemo(
    () => collections.filter((c: Collection) => !c.isSmart),
    [collections]
  );

  const getCollectionPath = (col: Collection): string => {
    const parts = [col.name];
    let curr = col;
    while (curr.parentId) {
      const parent = regularCollections.find((c: Collection) => c.id === curr.parentId);
      if (!parent) break;
      parts.unshift(parent.name);
      curr = parent;
    }
    return parts.join(' / ');
  };

  return (
    <div className={styles.tagsGrid}>
      {regularCollections.length === 0 && (
        <p className={styles.noTags}>No collections yet. Create collections in the sidebar.</p>
      )}
      {regularCollections.map((col: Collection) => {
        const isMember =
          (col.assetIds && col.assetIds.includes(asset.id)) ||
          (asset.collections?.includes(col.id) ?? false);
        const pathName = getCollectionPath(col);
        return (
          <button
            key={col.id}
            id={`detail-col-${col.id}`}
            className={clsx(styles.tagPill, isMember && styles.tagActive)}
            onClick={() => (isMember ? onRemove(col.id) : onAdd(col.id))}
            title={pathName}
          >
            <span className={styles.tagDot} style={{ background: 'var(--color-accent)' }} />
            {pathName}
            <span className={styles.tagCount}>({col.assetIds.length})</span>
          </button>
        );
      })}
    </div>
  );
}

// ── Preview Tab ───────────────────────────────────────────────────────────
function PreviewTab({ asset, onGeneratePreview }: { asset: Asset; onGeneratePreview: () => void }) {
  const modelPath =
    (asset.metadata?.preview_model as string) ||
    (asset.thumbnailPath?.endsWith('.glb') ? asset.thumbnailPath : undefined);
  // The original where the webview can display it, as in the header preview
  const imagePath = imageOf(asset);
  const isPending = asset.previewStatus === 'pending' || asset.previewStatus === 'generating';
  const isError = asset.previewStatus === 'error';
  const scaling = useImageScaling('contain');

  return (
    <div className={styles.previewArea}>
      {modelPath ? (
        <div className={styles.previewViewerWrapper}>
          <ThreeViewer modelPath={modelPath} />
          <button
            className={styles.reGenerateOverlayBtn}
            onClick={onGeneratePreview}
            disabled={isPending}
            title="Re-generate preview"
          >
            <IconRefresh size={13} className={clsx(isPending && 'animate-spin')} />
            <span>{isPending ? 'Generating…' : 'Regenerate Preview'}</span>
          </button>
        </div>
      ) : asset.kind === 'video' && asset.filePath ? (
        <div className={styles.previewViewerWrapper}>
          <video
            src={convertFileSrc(asset.filePath)}
            controls
            autoPlay
            muted
            playsInline
            className={styles.previewImg}
            style={{ width: '100%', height: '100%', objectFit: 'contain', background: '#000' }}
          />
        </div>
      ) : imagePath ? (
        <div className={styles.previewViewerWrapper}>
          <img
            src={convertFileSrc(imagePath)}
            alt={asset.fileName}
            className={styles.previewImg}
            ref={scaling}
          />
          <button
            className={styles.reGenerateOverlayBtn}
            onClick={onGeneratePreview}
            disabled={isPending}
            title="Re-generate preview"
          >
            <IconRefresh size={13} className={clsx(isPending && 'animate-spin')} />
            <span>{isPending ? 'Generating…' : 'Regenerate Preview'}</span>
          </button>
        </div>
      ) : (
        <div className={styles.previewEmpty}>
          <div className={styles.emptyStatusIcon}>
            {isPending ? (
              <span className={clsx(styles.spinnerBig, 'animate-spin')}>⟳</span>
            ) : isError ? (
              <span className={styles.errorExclamation}>!</span>
            ) : (
              <IconEye size={36} />
            )}
          </div>
          <p className={styles.emptyStatusTitle}>
            {isPending
              ? 'Generating preview…'
              : isError
                ? 'Preview generation failed previously'
                : 'No preview available'}
          </p>
          <p className={styles.emptyStatusSub}>
            {isPending
              ? 'Please wait while the worker renders the model'
              : 'Click below to run the preview engine worker for this file'}
          </p>
          <button
            id="detail-generate-preview"
            className={clsx(styles.generateBtn, isPending && styles.generateBtnLoading)}
            onClick={onGeneratePreview}
            disabled={isPending}
          >
            <IconRefresh size={14} className={clsx(isPending && 'animate-spin')} />
            <span>
              {isPending ? 'Generating…' : isError ? 'Retry Generate Preview' : 'Generate Preview'}
            </span>
          </button>
        </div>
      )}
    </div>
  );
}

// ── Preview ───────────────────────────────────────────────────────────────
/**
 * The panel's preview of `asset`. Without `onGeneratePreview`, as for a group member, which the
 * preview queue skips, a file with no preview shows only its extension.
 */
function AssetPreview({
  asset,
  onGeneratePreview,
}: {
  asset: Asset;
  onGeneratePreview?: () => void;
}) {
  const scaling = useImageScaling('contain');
  const image = imageOf(asset);
  const isPending = asset.previewStatus === 'pending' || asset.previewStatus === 'generating';
  const isError = asset.previewStatus === 'error';

  if (image) {
    return (
      <img
        src={convertFileSrc(image)}
        alt={asset.fileName}
        className={styles.thumbImg}
        ref={scaling}
      />
    );
  }
  if (asset.kind === 'video' && asset.filePath) {
    return (
      <video
        src={convertFileSrc(asset.filePath)}
        preload="metadata"
        muted
        playsInline
        controls
        className={styles.thumbImg}
        style={{ objectFit: 'contain' }}
        onLoadedMetadata={(e) => {
          try {
            e.currentTarget.currentTime = 0.1;
          } catch {}
        }}
      />
    );
  }
  if (asset.metadata?.preview_model || asset.thumbnailPath?.endsWith('.glb')) {
    return (
      <ThreeViewer modelPath={(asset.metadata?.preview_model as string) || asset.thumbnailPath!} />
    );
  }
  return (
    <div className={styles.thumbPlaceholder}>
      <span className={styles.thumbExt}>.{asset.extension}</span>
      {onGeneratePreview && (
        <button
          className={styles.thumbGenerateBtn}
          onClick={onGeneratePreview}
          disabled={isPending}
          title={isError ? 'Preview failed. Click to retry' : 'Generate preview'}
        >
          <IconRefresh size={12} className={clsx(isPending && 'animate-spin')} />
          <span>{isPending ? 'Generating…' : isError ? 'Retry' : 'Generate'}</span>
        </button>
      )}
    </div>
  );
}

// ── File actions ──────────────────────────────────────────────────────────
/** Open, Reveal and Copy path on one file. With `idPrefix`, the buttons get ids under it. */
function FileActions({ filePath, idPrefix }: { filePath: string; idPrefix?: string }) {
  const id = (name: string) => (idPrefix ? `${idPrefix}-${name}` : undefined);
  return (
    <>
      <button
        id={id('open-external')}
        data-file-action="open"
        className={styles.actionBtn}
        title="Open with default app"
        onClick={async (e) => {
          e.stopPropagation();
          try {
            await api.openWithDefault(filePath);
          } catch (err) {
            console.error('Failed to open with default app:', err);
          }
        }}
      >
        <IconExternal size={14} />
      </button>
      <button
        id={id('reveal')}
        data-file-action="reveal"
        className={styles.actionBtn}
        title="Reveal in file manager"
        onClick={async (e) => {
          e.stopPropagation();
          try {
            await api.revealInFileManager(filePath);
          } catch (err) {
            console.error('Failed to reveal in file manager:', err);
          }
        }}
      >
        <IconEye size={14} />
      </button>
      <button
        id={id('copy-path')}
        data-file-action="copy-path"
        className={styles.actionBtn}
        title="Copy path"
        onClick={(e) => {
          e.stopPropagation();
          navigator.clipboard.writeText(filePath);
        }}
      >
        <IconCopy size={14} />
      </button>
    </>
  );
}

// ── Group files ───────────────────────────────────────────────────────────
/**
 * A format group's files, main item first, each with its format, folder and size and its own
 * file actions. Clicking one shows its details in the panel.
 */
function GroupFiles({ members, onShow }: { members: GroupMember[]; onShow: (id: string) => void }) {
  return (
    <section className={styles.groupFiles} id="detail-group-files">
      <p className={styles.groupFilesTitle}>Files ({members.length})</p>
      <ul className={styles.groupFileList}>
        {members.map((m, i) => {
          const image = imageOf(m);
          const folder = m.folder || '/';
          return (
            <li
              key={m.id}
              data-group-member={m.id}
              className={styles.groupFile}
              role="button"
              tabIndex={0}
              title={`Show ${m.fileName}`}
              onClick={() => onShow(m.id)}
              onKeyDown={(e) => {
                // A key on the row's own file actions is theirs: Enter or Space runs that action
                if (e.target !== e.currentTarget) return;
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  onShow(m.id);
                }
              }}
            >
              <span className={styles.groupFileThumb}>
                {image ? (
                  <img src={convertFileSrc(image)} alt="" loading="lazy" />
                ) : (
                  <span className={styles.groupFileExt}>{m.extension.toUpperCase()}</span>
                )}
              </span>
              <span className={styles.groupFileText}>
                <span className={styles.groupFileFormat}>
                  {m.extension.toUpperCase()}
                  {i === 0 && <span className={styles.groupFileMain}>main</span>}
                  <span className={styles.groupFileSize}>{formatSize(m.sizeBytes)}</span>
                </span>
                <span className={clsx(styles.groupFileFolder, 'truncate')} title={folder}>
                  {/* Isolated, so the right-to-left box keeps the path's own order */}
                  <bdi>{folder}</bdi>
                </span>
              </span>
              <span className={styles.groupFileActions}>
                <FileActions filePath={m.filePath} />
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
