import { useEffect, useState, useRef } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Sidebar } from './components/Sidebar/Sidebar';
import { Toolbar } from './components/Toolbar/Toolbar';
import { AssetGrid } from './components/AssetGrid/AssetGrid';
import { DetailPanel } from './components/DetailPanel/DetailPanel';
import { StatusBar } from './components/StatusBar/StatusBar';
import { QuickLookModal } from './components/QuickLook/QuickLookModal';
import { ConfirmDeleteDialog } from './components/ConfirmDeleteDialog';
import { RunsPanel } from './components/Autotag/RunsPanel';
import { AutotagDialog } from './components/Autotag/AutotagDialog';
import { ReviewModal } from './components/Autotag/ReviewModal';
import { IconCheck } from './components/Icons';
import { useUIStore } from './store';
import {
  useLibraryStore,
  useTagStore,
  useCollectionStore,
  useAssetStore,
  useAutotagStore,
  useFolderTreeStore,
  usePacksStore,
  usePackDetailStore,
  useLiveCountsStore,
  lookupAsset,
} from './store';
import type { AutotagUpdate } from './types';
import { api } from './api';
import { startModelThumbnailer } from './preview/modelThumbnailer';
import { isEditingText, isInControl } from './utils/keyboard';
import { itemFileCount } from './utils/format';
import styles from './App.module.css';

export default function App() {
  const { sidebarOpen, detailPanelOpen, focusedAssetId } = useUIStore();
  const focusedAsset = useAssetStore((s) =>
    focusedAssetId ? (lookupAsset(s, focusedAssetId) ?? null) : null
  );
  const fetchLibraries = useLibraryStore((s) => s.fetchLibraries);
  const fetchTags = useTagStore((s) => s.fetchTags);
  const fetchCollections = useCollectionStore((s) => s.fetchCollections);
  const search = useAssetStore((s) => s.search);
  const runsPanelOpen = useAutotagStore((s) => s.runsPanelOpen);
  const autotagTarget = useAutotagStore((s) => s.dialogTarget);
  const openAutotagDialog = useAutotagStore((s) => s.openAutotagDialog);
  const reviewRunId = useAutotagStore((s) => s.reviewRunId);
  const assetsToDelete = useUIStore((s) => s.assetsToDelete);
  const requestDelete = useUIStore((s) => s.requestDelete);

  const [quickLookOpen, setQuickLookOpen] = useState(false);
  const [copyToast, setCopyToast] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const toastTimeoutRef = useRef<number | null>(null);

  useEffect(() => {
    // Bootstrap data on mount
    Promise.all([fetchLibraries(), fetchTags(), fetchCollections()]).then(() => search());

    // Real-time preview updates from background engine
    const unlistenDone = listen<{
      assetId: string;
      filePath?: string | null;
      previousThumbnailPath?: string | null;
      thumbnailPath: string | null;
      previewModel?: string;
    }>('preview:done', (event) => {
      useAssetStore.getState().updateAssetPreview(
        event.payload.assetId,
        event.payload.thumbnailPath,
        event.payload.previewModel,
        event.payload.filePath
          ? {
              filePath: event.payload.filePath,
              previousThumbnailPath: event.payload.previousThumbnailPath ?? undefined,
            }
          : undefined
      );
    });

    const unlistenError = listen<{ assetId: string }>('preview:error', (event) => {
      useAssetStore.getState().setPreviewStatus(event.payload.assetId, 'error');
    });

    // Render grid thumbnails for 3D models the preview engine converted to .glb
    const stopModelThumbnailer = startModelThumbnailer();

    // Auto-tagging runs: the first list, then the engine pushes every change
    useAutotagStore.getState().fetchRuns().catch(console.error);
    useAutotagStore.getState().fetchProviders().catch(console.error);
    useAutotagStore.getState().fetchLastUsed().catch(console.error);
    const unlistenAutotag = listen<AutotagUpdate>('autotag:updated', (event) => {
      useAutotagStore.getState().applyUpdate(event.payload);
    });

    // Live counts follow the results: once per search or refresh, like the total
    const stopLiveCounts = useAssetStore.subscribe((s, prev) => {
      if (s.epoch !== prev.epoch) useLiveCountsStore.getState().recount(s.query);
    });

    const unlistenLib = listen('library:updated', () => {
      useAssetStore.getState().refresh();
      useLibraryStore.getState().fetchLibraries();
      useFolderTreeStore.getState().refetch();
      usePacksStore.getState().refetch();
      usePackDetailStore.getState().refetch();
    });

    // Keyboard shortcuts: F5, Space hold for QuickLook, Ctrl+C to copy path
    const handleKeyDown = (e: KeyboardEvent) => {
      // Refresh
      if (e.key === 'F5' || (e.ctrlKey && e.key.toLowerCase() === 'r')) {
        e.preventDefault();
        api
          .refreshAllLibraries()
          .then(() => {
            useAssetStore.getState().search();
            useLibraryStore.getState().fetchLibraries();
          })
          .catch(console.error);
        return;
      }

      // Copy path with Ctrl+C / Cmd+C when asset is selected
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c' && !isEditingText()) {
        const selectedIds = useUIStore.getState().selectedAssetIds;
        const focusedId = useUIStore.getState().focusedAssetId;
        const ids = selectedIds.size > 0 ? Array.from(selectedIds) : focusedId ? [focusedId] : [];
        if (ids.length > 0) {
          const paths = ids
            .map((id) => useAssetStore.getState().getById(id)?.filePath)
            .filter(Boolean) as string[];
          if (paths.length > 0) {
            e.preventDefault();
            navigator.clipboard.writeText(paths.join('\n'));
            setCopyToast(
              paths.length === 1
                ? 'Copied file path to clipboard'
                : `Copied ${paths.length} file paths to clipboard`
            );
            if (toastTimeoutRef.current) clearTimeout(toastTimeoutRef.current);
            toastTimeoutRef.current = window.setTimeout(() => setCopyToast(null), 2200);
          }
        }
      }

      // Toggle favorite on focused/selected asset(s) with 'f' or 'F'
      if (
        (e.key === 'f' || e.key === 'F') &&
        !e.ctrlKey &&
        !e.metaKey &&
        !e.altKey &&
        !isEditingText()
      ) {
        const selectedIds = useUIStore.getState().selectedAssetIds;
        const focusedId = useUIStore.getState().focusedAssetId;
        const ids = selectedIds.size > 0 ? Array.from(selectedIds) : focusedId ? [focusedId] : [];
        if (ids.length > 0) {
          e.preventDefault();
          const assetStore = useAssetStore.getState();
          const firstAsset = assetStore.getById(ids[0]);
          const newFav = !(firstAsset?.isFavorite ?? false);
          for (const id of ids) {
            assetStore.patchAsset(id, { isFavorite: newFav });
          }
          setCopyToast(
            ids.length === 1
              ? newFav
                ? 'Added to favorites'
                : 'Removed from favorites'
              : newFav
                ? `Added ${ids.length} assets to favorites`
                : `Removed ${ids.length} assets from favorites`
          );
          if (toastTimeoutRef.current) clearTimeout(toastTimeoutRef.current);
          toastTimeoutRef.current = window.setTimeout(() => setCopyToast(null), 2200);
        }
      }

      // Delete selected asset(s) with Delete or Backspace key
      if ((e.key === 'Delete' || e.key === 'Backspace') && !isEditingText()) {
        const selectedIds = useUIStore.getState().selectedAssetIds;
        const focusedId = useUIStore.getState().focusedAssetId;
        const ids = selectedIds.size > 0 ? Array.from(selectedIds) : focusedId ? [focusedId] : [];
        if (ids.length > 0) {
          e.preventDefault();
          useUIStore.getState().requestDelete(ids);
          return;
        }
      }

      // F2 to trigger rename on focused asset
      if (e.key === 'F2' && !isEditingText()) {
        const focusedId = useUIStore.getState().focusedAssetId;
        if (focusedId) {
          e.preventDefault();
          const renameBtn = document.getElementById('detail-rename');
          renameBtn?.click();
          return;
        }
      }

      // Hold Spacebar for Quick Look preview
      // A focused control, such as a card's favorite button, keeps Space for its own activation
      if (e.code === 'Space' && !isEditingText() && !isInControl(e.target)) {
        const focusedId = useUIStore.getState().focusedAssetId;
        if (focusedId) {
          e.preventDefault();
          if (!e.repeat) {
            setQuickLookOpen(true);
          }
        }
      }

      // Escape closes Quick Look
      if (e.key === 'Escape') {
        setQuickLookOpen(false);
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      if (e.code === 'Space') {
        setQuickLookOpen(false);
      }
    };

    const handleBlur = () => {
      setQuickLookOpen(false);
    };

    window.addEventListener('keydown', handleKeyDown);
    window.addEventListener('keyup', handleKeyUp);
    window.addEventListener('blur', handleBlur);

    return () => {
      unlistenDone.then((fn) => fn());
      unlistenError.then((fn) => fn());
      unlistenLib.then((fn) => fn());
      unlistenAutotag.then((fn) => fn());
      stopModelThumbnailer();
      stopLiveCounts();
      window.removeEventListener('keydown', handleKeyDown);
      window.removeEventListener('keyup', handleKeyUp);
      window.removeEventListener('blur', handleBlur);
      if (toastTimeoutRef.current) clearTimeout(toastTimeoutRef.current);
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // A group's members and its bundled preview go with it (plan C21)
  const deleteFileCount = assetsToDelete.reduce((n, id) => {
    const asset = useAssetStore.getState().getById(id);
    return n + (asset ? itemFileCount(asset) : 1);
  }, 0);

  const handleConfirmAppDelete = async () => {
    const ids = assetsToDelete;
    if (ids.length === 0) return;
    const count = deleteFileCount;
    requestDelete([]);
    setDeleteError(null);
    try {
      await useAssetStore.getState().deleteAssets(ids, true);
      setCopyToast(
        count === 1 ? 'Moved file to Recycle Bin' : `Moved ${count} files to Recycle Bin`
      );
      if (toastTimeoutRef.current) clearTimeout(toastTimeoutRef.current);
      toastTimeoutRef.current = window.setTimeout(() => setCopyToast(null), 2200);
    } catch (err: any) {
      setDeleteError(err?.message || String(err));
    }
  };

  return (
    <div className={styles.layout}>
      <Toolbar />
      <div className={styles.body}>
        {sidebarOpen && <Sidebar />}
        <main className={styles.main}>
          <AssetGrid />
        </main>
        {detailPanelOpen && <DetailPanel />}
      </div>
      <StatusBar />

      {/* Spacebar Quick Look Modal */}
      <QuickLookModal asset={focusedAsset} isOpen={quickLookOpen} />
      {runsPanelOpen && <RunsPanel />}
      {reviewRunId && <ReviewModal key={reviewRunId} runId={reviewRunId} />}
      {autotagTarget && (
        <AutotagDialog target={autotagTarget} onClose={() => openAutotagDialog(null)} />
      )}

      {/* Delete Confirmation Dialog */}
      <ConfirmDeleteDialog
        isOpen={assetsToDelete.length > 0}
        itemType={assetsToDelete.length > 1 ? 'assets' : 'asset'}
        itemName={
          assetsToDelete.length === 1
            ? useAssetStore.getState().getById(assetsToDelete[0])?.fileName || '1 file'
            : `${assetsToDelete.length} ${deleteFileCount === assetsToDelete.length ? 'files' : 'items'}`
        }
        fileCount={deleteFileCount}
        onConfirm={handleConfirmAppDelete}
        onCancel={() => requestDelete([])}
      />

      {/* Copy Path Feedback Toast */}
      {copyToast && (
        <div className={styles.copyToast}>
          <IconCheck size={14} className={styles.toastCheck} />
          <span>{copyToast}</span>
        </div>
      )}

      {deleteError && (
        <div className={`${styles.copyToast} ${styles.errorToast}`} role="alert">
          <span>{deleteError}</span>
          <button className={styles.toastDismiss} onClick={() => setDeleteError(null)}>
            Dismiss
          </button>
        </div>
      )}
    </div>
  );
}
