import { invoke } from '@tauri-apps/api/core';
import type {
  Asset,
  Library,
  Collection,
  Tag,
  TagPatch,
  AddLibraryPayload,
  SearchPayload,
  SearchResult,
  PreviewJob,
  PendingModelThumbnail,
  PreviewProgress,
  AiProvider,
  AiSettings,
  AiSettingsPatch,
  ProviderStatus,
  AutotagMode,
  AutotagProviderOption,
  RunSummary,
  StartedRun,
} from '../types';

/** Thin wrapper around Tauri IPC commands */
export const api = {
  // Libraries
  addLibrary: (payload: AddLibraryPayload) => invoke<Library>('add_library', { payload }),
  listLibraries: () => invoke<Library[]>('list_libraries'),
  removeLibrary: (id: string) => invoke<void>('remove_library', { id }),
  refreshLibrary: (id: string) => invoke<void>('refresh_library', { id }),
  refreshAllLibraries: () => invoke<void>('refresh_all_libraries'),

  // Assets
  searchAssets: (payload: SearchPayload) => invoke<SearchResult>('search_assets', { payload }),
  getAsset: (id: string) => invoke<Asset>('get_asset', { id }),
  updateAsset: (id: string, patch: Partial<Asset>) => invoke<Asset>('update_asset', { id, patch }),
  deleteAsset: (id: string, deleteFile?: boolean) =>
    invoke<void>('delete_asset', { id, deleteFile: deleteFile ?? true }),
  deleteAssets: (ids: string[], deleteFile?: boolean) =>
    invoke<void>('delete_assets', { ids, deleteFile: deleteFile ?? true }),
  renameAsset: (id: string, newName: string) => invoke<Asset>('rename_asset', { id, newName }),

  // Tags
  listTags: () => invoke<Tag[]>('list_tags'),
  createTag: (name: string, parentId?: string) => invoke<Tag>('create_tag', { name, parentId }),
  updateTag: (id: string, patch: TagPatch) => invoke<Tag>('update_tag', { id, patch }),
  deleteTag: (id: string) => invoke<void>('delete_tag', { id }),

  // Collections
  listCollections: () => invoke<Collection[]>('list_collections'),
  createCollection: (
    name: string,
    description?: string,
    parentId?: string,
    isSmart?: boolean,
    filterSpec?: string
  ) =>
    invoke<Collection>('create_collection', {
      name,
      description,
      parentId,
      isSmart,
      filterSpec,
    }),
  addToCollection: (collectionId: string, assetIds: string[]) =>
    invoke<void>('add_to_collection', { collectionId, assetIds }),
  removeFromCollection: (collectionId: string, assetIds: string[]) =>
    invoke<void>('remove_from_collection', { collectionId, assetIds }),
  deleteCollection: (id: string) => invoke<void>('delete_collection', { id }),

  // Preview
  requestPreview: (assetIds: string[]) => invoke<void>('request_preview', { assetIds }),
  getPreviewJobs: () => invoke<PreviewJob[]>('get_preview_jobs'),
  getPreviewProgress: () => invoke<PreviewProgress>('get_preview_progress'),
  listPendingModelThumbnails: (limit: number) =>
    invoke<PendingModelThumbnail[]>('list_pending_model_thumbnails', { limit }),
  /** Sends the PNG as a raw body; returns the saved thumbnail's path. */
  saveModelThumbnail: (assetId: string, png: Uint8Array) =>
    invoke<string>('save_model_thumbnail', png, { headers: { 'asset-id': assetId } }),

  // Reveal in file manager / open with default app
  revealInFileManager: (filePath: string) => invoke<void>('reveal_in_file_manager', { filePath }),
  openWithDefault: (filePath: string) => invoke<void>('open_with_default', { filePath }),

  // AI tagging settings. Keys are write-only: no command returns one.
  getAiSettings: () => invoke<AiSettings>('get_ai_settings'),
  setAiSettings: (patch: AiSettingsPatch) => invoke<AiSettings>('set_ai_settings', { patch }),
  /** Saves `key` in the OS keyring, or clears the saved key when `key` is null */
  setApiKey: (provider: AiProvider, key: string | null) =>
    invoke<ProviderStatus>('set_api_key', { provider, key }),
  getProviderStatus: () => invoke<ProviderStatus[]>('get_provider_status'),

  // Auto-tagging runs. Progress arrives as `autotag:updated` events.
  listAutotagProviders: () => invoke<AutotagProviderOption[]>('list_autotag_providers'),
  startAutotagRun: (assetIds: string[], provider: string, mode: AutotagMode) =>
    invoke<StartedRun>('start_autotag_run', { assetIds, provider, mode }),
  listAutotagRuns: () => invoke<RunSummary[]>('list_autotag_runs'),
};
