// ── Asset types ─────────────────────────────────────────────────────────
export type AssetKind =
  | '3d_model'
  | 'material'
  | 'texture'
  | 'hdri'
  | 'ies'
  | 'video'
  | 'image'
  | 'document'
  | 'other';

export type ColorLabel =
  | 'none'
  | 'red'
  | 'orange'
  | 'yellow'
  | 'green'
  | 'teal'
  | 'blue'
  | 'purple'
  | 'pink';

export interface Tag {
  id: string;
  name: string;
  color?: ColorLabel;
  parentId?: string;
  /** Hint sent to the tagging providers with the tag. */
  description: string;
}

/** Changes to a tag. A field left out stays as it is; `null` clears `color` or `parentId`. */
export interface TagPatch {
  name?: string;
  color?: ColorLabel | null;
  parentId?: string | null;
  description?: string;
}

export interface AssetMeta {
  /** Format-specific extracted metadata (renderer, poly count, resolution…) */
  [key: string]: unknown;
}

export interface Asset {
  id: string;
  /** Absolute path on the local filesystem */
  filePath: string;
  fileName: string;
  /** File extension, lowercase, no dot */
  extension: string;
  kind: AssetKind;
  /** Size in bytes */
  sizeBytes: number;
  /** Last-modified timestamp (ms) */
  modifiedAt: number;
  /** When it was added to the library */
  indexedAt: number;
  /** Star rating 0–5 */
  rating: number;
  colorLabel: ColorLabel;
  isFavorite: boolean;
  description: string;
  tags: string[]; // tag IDs
  collections?: string[]; // collection IDs
  /** Relative path to cached thumbnail (inside DAM cache dir) */
  thumbnailPath?: string;
  /** Whether a preview job is pending/running */
  previewStatus: 'none' | 'pending' | 'generating' | 'done' | 'error';
  metadata: AssetMeta;
}

export interface Library {
  id: string;
  name: string;
  /** Root paths this library watches */
  rootPaths: string[];
  createdAt: number;
  assetCount: number;
}

export interface Collection {
  id: string;
  name: string;
  description: string;
  parentId?: string;
  assetIds: string[];
  isSmart: boolean;
  /** JSON-serialized filter spec for smart collections */
  filterSpec?: string;
  createdAt: number;
}

// ── Search & Filter ──────────────────────────────────────────────────────
export type SortField = 'name' | 'date' | 'size' | 'rating' | 'kind' | 'favorite';
export type SortOrder = 'asc' | 'desc';

export interface FilterSpec {
  kinds?: AssetKind[];
  tags?: string[];
  rating?: { min: number; max: number };
  colorLabels?: ColorLabel[];
  isFavorite?: boolean;
  dateRange?: { from?: number; to?: number };
  sizeRange?: { min?: number; max?: number };
  extensions?: string[];
  previewStatus?: Asset['previewStatus'];
  unorganized?: boolean;
  directory?: string;
  namePrefix?: string;
  nameSuffix?: string;
  tagLogic?: 'and' | 'or' | 'and_or' | 'none';
  collectionId?: string;
  collectionIds?: string[];
  collectionLogic?: 'and' | 'or' | 'and_or' | 'none';
  includeSubcollections?: boolean;
}

export interface SearchQuery {
  text: string;
  filter: FilterSpec;
  sort: { field: SortField; order: SortOrder };
  libraryId?: string;
  libraryIds?: string[];
  collectionId?: string;
  includeSubcollections?: boolean;
}

// ── Preview job ──────────────────────────────────────────────────────────
export interface PreviewJob {
  id: string;
  assetId: string;
  status: 'queued' | 'running' | 'done' | 'error';
  progress?: number; // 0–100
  errorMessage?: string;
  createdAt: number;
}

/** Preview queue totals across all libraries */
export interface PreviewProgress {
  total: number;
  /** Jobs that finished, successfully or not */
  finished: number;
  modelsAwaitingRender: number;
}

/** A converted 3D model that still needs a rendered grid thumbnail */
export interface PendingModelThumbnail {
  assetId: string;
  modelPath: string;
}

// ── IPC / Command interfaces (Tauri invoke payloads) ─────────────────────
export interface AddLibraryPayload {
  name: string;
  rootPaths: string[];
}

export interface SearchPayload {
  query: SearchQuery;
  page: number;
  pageSize: number;
}

/** How `delete_assets` rejects when some assets were not deleted; the rest were. */
export interface DeleteAssetsError {
  message: string;
  failedIds: string[];
}

export interface SearchResult {
  assets: Asset[];
  total: number;
  page: number;
  pageSize: number;
}

// ── AI tagging settings ──────────────────────────────────────────────────
export type AiProvider = 'anthropic' | 'jev';

export interface AnthropicSettings {
  /** A Claude model id, one of ANTHROPIC_MODELS or a custom id */
  model: string;
  /** Requests in flight at once, shared by every run */
  concurrency: number;
  maxTags: number;
  maxProposals: number;
}

export interface JevSettings {
  concurrency: number;
  /** Tags scoring at least this (0-1) are kept */
  threshold: number;
  maxTags: number;
}

export interface AiSettings {
  anthropic: AnthropicSettings;
  jev: JevSettings;
}

export interface AiSettingsPatch {
  anthropic?: Partial<AnthropicSettings>;
  jev?: Partial<JevSettings>;
}

/** Where a provider's key comes from. It never carries the key. */
export interface ProviderStatus {
  provider: AiProvider;
  configured: boolean;
  keySource: 'env' | 'keyring' | 'none';
  /** "from ANTHROPIC_API_KEY", "saved in keyring" or "not set" */
  sourceLabel: string;
  envVar: string;
  /** A key is saved in the keyring, even if the env var overrides it */
  savedInKeyring: boolean;
  keyringProblem: string | null;
}

// ── Auto-tagging runs ────────────────────────────────────────────────────
export type AutotagMode = 'existing_only' | 'prefer_existing' | 'create_freely';

/** A tagging provider as the run dialog shows it */
export interface AutotagProviderOption {
  /** "anthropic", "jev", or in debug builds "fake" and "fake-text" */
  id: string;
  label: string;
  /** The modes the provider can do */
  modes: AutotagMode[];
  /** Why no run can start with it, when none can */
  unavailableReason: string | null;
}

export type RunStatus =
  | 'queued'
  | 'running'
  | 'paused'
  | 'awaiting_review'
  | 'done'
  | 'cancelled'
  | 'undoing'
  | 'undo_partial'
  | 'undone';

export interface RunSummary {
  id: string;
  label: string;
  provider: string;
  providerLabel: string;
  model: string | null;
  mode: AutotagMode;
  status: RunStatus;
  pauseReason: string | null;
  total: number;
  done: number;
  failed: number;
  running: number;
  queued: number;
  inputTokens: number;
  outputTokens: number;
  costUsd: number;
  /** Unix milliseconds */
  createdAt: number;
  finishedAt: number | null;
}

/** What a run covers: chosen assets, or a library's untagged assets (every asset with `allAssets`) */
export type AutotagScope =
  | { kind: 'assets'; assetIds: string[] }
  | { kind: 'library'; libraryId: string; allAssets: boolean };

/** What the run dialog was opened for: chosen assets, or a sidebar library */
export type AutotagTarget =
  | { kind: 'assets'; assetIds: string[] }
  | { kind: 'library'; libraryId: string; libraryName: string };

export interface StartedRun {
  run: RunSummary;
  queued: number;
  /** Assets left out because no open library holds them */
  skipped: number;
}

/** The payload of the `autotag:updated` event */
export interface AutotagUpdate {
  runs: RunSummary[];
  /** Assets whose tags changed since the last event */
  changedAssetIds: string[];
}
