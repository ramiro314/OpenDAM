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
