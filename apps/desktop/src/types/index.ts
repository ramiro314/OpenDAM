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
  /**
   * For the main item of a group (one model in several formats, a file's copies under format
   * folders, or a sprite's scale and format copies in a 2D stack), every member's extension, its
   * own first. Absent for a file on its own.
   */
  groupFormats?: string[];
  /**
   * For a model with a bundled preview (a render shipped beside it, such as Kenney's
   * `Previews/<name>.png`), the picture to show: the render's thumbnail, or the render itself.
   * Cards show it before `thumbnailPath`.
   */
  bundledPreview?: string;
}

/** One file of a format group, as the detail panel lists it (`get_group_members`) */
export interface GroupMember extends Asset {
  /** The folder holding the file, relative to its library root; `''` is the root */
  folder: string;
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
  /** A folder path relative to the root of each library searched; `''` is the root */
  directory?: string;
  namePrefix?: string;
  nameSuffix?: string;
  tagLogic?: 'and' | 'or' | 'and_or' | 'none';
  collectionId?: string;
  collectionIds?: string[];
  collectionLogic?: 'and' | 'or' | 'and_or' | 'none';
  includeSubcollections?: boolean;
  /** Whether packs' support files (licenses, readmes, preview and sample images) show too */
  showHidden?: boolean;
}

export interface SearchQuery {
  text: string;
  filter: FilterSpec;
  sort: { field: SortField; order: SortOrder };
  libraryId?: string;
  libraryIds?: string[];
  collectionId?: string;
  includeSubcollections?: boolean;
  /** One folder of one library: only that library's files below the folder, recursively */
  folder?: FolderRef;
}

/** A library folder by its path relative to the library root; the root is `''` */
export interface FolderRef {
  libraryId: string;
  path: string;
}

/** One node of a library's folder tree (`get_folders`) */
export interface FolderNode {
  name: string;
  /** Relative to the library root */
  path: string;
  isPack: boolean;
  /** Visible files below the folder, at any depth */
  itemCount: number;
  /** Support files below the folder, at any depth, hidden unless "Show hidden files" is on */
  hiddenCount: number;
  hasChildren: boolean;
  /**
   * Whether the folder shows with hidden files off: it has visible files, is a pack or holds one.
   * `get_folders` owns the rule, for this and for `hasVisibleChildren`.
   */
  shownWithoutHidden: boolean;
  /** Whether a child shows with hidden files off */
  hasVisibleChildren: boolean;
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
  /** Whether to count every match into `SearchResult.total`; true when omitted. */
  includeTotal?: boolean;
}

/** How `delete_assets` rejects when some assets were not deleted; the rest were. */
export interface DeleteAssetsError {
  message: string;
  failedIds: string[];
}

export interface SearchResult {
  assets: Asset[];
  /** Every match, not only this page's; absent when the payload set `includeTotal` to false. */
  total?: number;
  /** The files the matches stand for, hidden support files included, counted with `total` */
  fileTotal?: number;
  page: number;
  pageSize: number;
}

// ── AI tagging settings ──────────────────────────────────────────────────
export type AiProvider = 'anthropic' | 'jev';

/** How Anthropic requests reach Claude: an API key, or your Claude plan through `claude` */
export type AnthropicAccess = 'api_key' | 'claude_plan';

export interface AnthropicSettings {
  access: AnthropicAccess;
  /** A Claude model id, one of ANTHROPIC_MODELS or a custom id. It applies to both accesses */
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

/** Claude-plan access: `claude`'s path, version and login, read as runs see them */
export interface ClaudePlanStatus {
  path: string | null;
  /** "2.1.296 (Claude Code)" */
  version: string | null;
  loggedIn: boolean;
  /** "claude.ai" when runs use your Claude plan */
  authMethod: string | null;
  /** "max", "pro", ... */
  subscriptionType: string | null;
  configured: boolean;
  /** Why Claude-plan runs can't start, when they can't */
  problem: string | null;
  /** Auth overrides set for the app that `claude` never gets, such as ANTHROPIC_API_KEY */
  ignoredOverrides: string[];
}

/** The result of "Test connection". It never carries the key. */
export interface ConnectionTest {
  ok: boolean;
  model: string;
  message: string;
  /** The test tags the model picked, as paths */
  tags: string[];
  inputTokens: number;
  outputTokens: number;
  costUsd: number;
  /** Sent through `claude` on your Claude plan, so it has no cost of its own */
  onPlan: boolean;
}

// ── Auto-tagging runs ────────────────────────────────────────────────────
export type AutotagMode = 'existing_only' | 'prefer_existing' | 'create_freely';

/** A tagging provider as the run dialog shows it */
export interface AutotagProviderOption {
  /** "anthropic", "jev", or in debug builds "fake" and "fake-text" */
  id: string;
  label: string;
  /** How a run started now reaches it, for providers with more than one way */
  access: AnthropicAccess | null;
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
  /** "api_key" or "claude_plan" for Anthropic runs */
  access: AnthropicAccess | null;
  model: string | null;
  mode: AutotagMode;
  status: RunStatus;
  pauseReason: string | null;
  /** What the provider last reported without pausing, such as Claude plan usage */
  notice: string | null;
  /** Started by "Suggest tags" on one asset */
  inline: boolean;
  total: number;
  done: number;
  failed: number;
  /** Items a cancel left unprocessed */
  skipped: number;
  running: number;
  queued: number;
  /** New tag names waiting for review */
  pendingProposals: number;
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
  /** Assets left out because another active run already has them queued */
  skipped: number;
  /** Assets left out because no open library holds them */
  missing: number;
}

/** What a run would use: a cost with an API key, or a request count on the Claude plan */
export type AutotagEstimate =
  | { kind: 'cost'; usd: number | null; model: string | null }
  | { kind: 'requests'; count: number };

/** What the run dialog shows before Start */
export interface RunEstimate {
  /** The assets the run would tag */
  assets: number;
  /** Assets it would leave out because another active run already has them queued */
  skipped: number;
  /** Assets it would leave out because no open library holds them */
  missing: number;
  estimate: AutotagEstimate;
}

/** The provider and mode of the last run started, which "Suggest tags" uses */
export interface AutotagLastUsed {
  provider: string;
  mode: AutotagMode;
}

/** A "Suggest tags" run as the asset's Tags tab shows it */
export interface AutotagSuggestion {
  run: RunSummary;
  /** Why the asset's item failed, when it did */
  error: string | null;
  /** The new tag names waiting for review, shown as chips */
  proposals: ReviewProposal[];
}

/** The payload of the `autotag:updated` event */
export interface AutotagUpdate {
  runs: RunSummary[];
  /** Assets whose tags changed since the last event */
  changedAssetIds: string[];
  /** A run created tags since the last event, so the tag catalog changed */
  tagsChanged: boolean;
}

/** An asset a proposed tag is for, as the review shows it */
export interface ReviewAsset {
  assetId: string;
  /** null when its library is offline or the asset is gone */
  fileName: string | null;
  thumbnailPath: string | null;
  included: boolean;
}

/** A new tag name a Prefer existing run collected, waiting for review */
export interface ReviewProposal {
  id: string;
  name: string;
  /** The catalog tag the model suggested as its parent */
  parentId: string | null;
  assets: ReviewAsset[];
}

/** What the review decided for one proposal; `assetIds` are the ticked assets */
export type ProposalDecision =
  | {
      action: 'accept';
      proposalId: string;
      name: string;
      parentId: string | null;
      assetIds: string[];
    }
  | { action: 'map'; proposalId: string; tagId: string; assetIds: string[] }
  | { action: 'reject'; proposalId: string };
