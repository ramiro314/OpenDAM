import { invoke } from '@tauri-apps/api/core';
import type {
  Asset,
  GroupMember,
  Library,
  Collection,
  FacetCounts,
  FolderCounts,
  FolderNode,
  PackCard,
  PackDetail,
  PackEdit,
  PackRef,
  Tag,
  TagPatch,
  AddLibraryPayload,
  SearchPayload,
  SearchQuery,
  SearchResult,
  PreviewJob,
  PendingModelThumbnail,
  PreviewProgress,
  AiProvider,
  AiSettings,
  AnthropicAccess,
  ClaudePlanStatus,
  AiSettingsPatch,
  ExportCheck,
  ExportSettings,
  ExportSettingsPatch,
  ProviderStatus,
  AutotagMode,
  ConnectionTest,
  AutotagProviderOption,
  AutotagScope,
  AutotagLastUsed,
  AutotagSuggestion,
  RunSummary,
  StartedRun,
  RunEstimate,
  ReviewProposal,
  ProposalDecision,
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
  /** The files of the group `id` belongs to, main item first; a file on its own alone */
  getGroupMembers: (id: string) => invoke<GroupMember[]>('get_group_members', { id }),
  updateAsset: (id: string, patch: Partial<Asset>) => invoke<Asset>('update_asset', { id, patch }),
  deleteAsset: (id: string, deleteFile?: boolean) =>
    invoke<void>('delete_asset', { id, deleteFile: deleteFile ?? true }),
  deleteAssets: (ids: string[], deleteFile?: boolean) =>
    invoke<void>('delete_assets', { ids, deleteFile: deleteFile ?? true }),
  renameAsset: (id: string, newName: string) => invoke<Asset>('rename_asset', { id, newName }),

  // Folder tree: the children of one node, `''` being the library root
  getFolders: (libraryId: string, parentPath: string) =>
    invoke<FolderNode[]>('get_folders', { libraryId, parentPath }),
  /** The packs below `underPath` in each of `libraryIds`, or in every library when null */
  getPacks: (libraryIds: string[] | null, underPath: string) =>
    invoke<PackCard[]>('get_packs', { libraryIds, underPath }),
  /** The pack holding `path`: the innermost pack at or above it, or null when none does */
  getPackDetail: (libraryId: string, path: string) =>
    invoke<PackDetail | null>('get_pack_detail', { libraryId, path }),
  /** Saves an edit to the folder's pack, reruns the structure pass, then emits `library:updated` */
  setPackOverride: (libraryId: string, path: string, edit: PackEdit) =>
    invoke<void>('set_pack_override', { libraryId, path, edit }),
  /** The pack "Use as pack cover" on the asset sets: the innermost one holding the image */
  getCoverPack: (assetId: string) => invoke<PackRef | null>('get_cover_pack', { assetId }),
  /** Tags every file below the folder and their groups' members; returns how many */
  tagFolder: (libraryId: string, folderPath: string, tagId: string) =>
    invoke<number>('tag_folder', { libraryId, folderPath, tagId }),
  /** Adds every file below the folder and their groups' members to a collection */
  collectFolder: (libraryId: string, folderPath: string, collectionId: string) =>
    invoke<number>('collect_folder', { libraryId, folderPath, collectionId }),

  // Live counts while search text or a filter is active, each the grid's total for its value
  getFacetCounts: (query: SearchQuery) => invoke<FacetCounts>('get_facet_counts', { query }),
  getFolderCounts: (query: SearchQuery, libraryId: string, parentPaths: string[]) =>
    invoke<FolderCounts>('get_folder_counts', { query, libraryId, parentPaths }),

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
  /** Opens an http(s) address, such as a pack's source URL, in the browser */
  openUrl: (url: string) => invoke<void>('open_url', { url }),

  // AI tagging settings. Keys are write-only: no command returns one.
  getAiSettings: () => invoke<AiSettings>('get_ai_settings'),
  setAiSettings: (patch: AiSettingsPatch) => invoke<AiSettings>('set_ai_settings', { patch }),
  /** Saves `key` in the OS keyring, or clears the saved key when `key` is null */
  setApiKey: (provider: AiProvider, key: string | null) =>
    invoke<ProviderStatus>('set_api_key', { provider, key }),
  getProviderStatus: () => invoke<ProviderStatus[]>('get_provider_status'),
  /** `claude`'s path, version and login, read from the cleaned environment runs use */
  getClaudePlanStatus: () => invoke<ClaudePlanStatus>('get_claude_plan_status'),
  /** One small request to `model` or the saved model, with the API key or on your Claude plan */
  testProvider: (provider: AiProvider, model?: string, access?: AnthropicAccess) =>
    invoke<ConnectionTest>('test_provider', {
      provider,
      model: model ?? null,
      access: access ?? null,
    }),

  // Game export settings. An export root in or around a library is refused.
  getExportSettings: () => invoke<ExportSettings>('get_export_settings'),
  setExportSettings: (patch: ExportSettingsPatch) =>
    invoke<ExportSettings>('set_export_settings', { patch }),
  /** Deletes every open library's export cache, returning how many it deleted */
  clearExportCache: () => invoke<number>('clear_export_cache'),
  /** Reads a domain collection and lists its assets and the problems that refuse its export */
  checkExport: (domainId: string) => invoke<ExportCheck>('check_export', { domainId }),

  // Auto-tagging runs. Progress arrives as `autotag:updated` events.
  listAutotagProviders: () => invoke<AutotagProviderOption[]>('list_autotag_providers'),
  estimateAutotagRun: (scope: AutotagScope, provider: string, mode: AutotagMode) =>
    invoke<RunEstimate>('estimate_autotag_run', { scope, provider, mode }),
  /** `inline` is "Suggest tags" on one asset, which sends no desktop notification */
  startAutotagRun: (scope: AutotagScope, provider: string, mode: AutotagMode, inline = false) =>
    invoke<StartedRun>('start_autotag_run', { scope, provider, mode, inline }),
  getAutotagLastUsed: () => invoke<AutotagLastUsed>('get_autotag_last_used'),
  getAutotagSuggestions: (assetId: string) =>
    invoke<AutotagSuggestion[]>('get_autotag_suggestions', { assetId }),
  listAutotagRuns: () => invoke<RunSummary[]>('list_autotag_runs'),
  pauseAutotagRun: (runId: string) => invoke<void>('pause_autotag_run', { runId }),
  /** Fails, leaving the run paused, when its provider isn't ready */
  resumeAutotagRun: (runId: string) => invoke<void>('resume_autotag_run', { runId }),
  /** Resumes every paused run whose provider is ready; returns how many */
  resumeAllAutotagRuns: () => invoke<number>('resume_all_autotag_runs'),
  cancelAutotagRun: (runId: string) => invoke<void>('cancel_autotag_run', { runId }),
  getAutotagProposals: (runId: string) =>
    invoke<ReviewProposal[]>('get_autotag_proposals', { runId }),
  /** Applies the review; returns the run's status afterwards */
  reviewAutotagProposals: (runId: string, decisions: ProposalDecision[]) =>
    invoke<string>('review_autotag_proposals', { runId, decisions }),
  /** Undo, or Finish undo on a partly undone run; returns `undone` or `undo_partial` */
  undoAutotagRun: (runId: string) => invoke<string>('undo_autotag_run', { runId }),
  /** Starts "Retry of <label>" over the run's failed assets; the run itself doesn't change */
  retryFailedAutotagRun: (runId: string) =>
    invoke<StartedRun>('retry_failed_autotag_run', { runId }),
  /** Clears one stopped run, or with none every finished run; returns how many it cleared */
  clearAutotagRuns: (runId: string | null = null) =>
    invoke<number>('clear_autotag_runs', { runId }),
};
