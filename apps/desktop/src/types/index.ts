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
   * `Previews/<name>.png`) that fits a card, the picture to show: the render's thumbnail, or the
   * render itself. Cards show it before `thumbnailPath`.
   */
  bundledPreview?: string;
  /**
   * For a model whose render is too small to fit a card (under 512 px on its shorter side), the
   * render's picture, as `bundledPreview`. Cards show it only after `thumbnailPath`.
   */
  fallbackRender?: string;
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
  /** A pack's cover image */
  cover?: Cover | null;
  /** What you said the folder is, if you did */
  mark?: PackMark | null;
  /** The name you gave the pack, shown in place of the folder's */
  renamed?: string | null;
}

/** "Mark as pack" or "Not a pack", against what the pack rules detect */
export type PackMark = 'pack' | 'not_pack';

/** Where a game export pack's files come from (plan game-export D12) */
export type PackOrigin = 'unity' | 'unreal' | 'generic';

/** A normal map's green channel convention (plan game-export D14): OpenGL is green up */
export type NormalConvention = 'opengl' | 'directx';

/** Your edits to one pack (`pack_overrides`); a null field keeps what was detected */
export interface PackOverride {
  mark?: PackMark | null;
  name?: string | null;
  /** The license as you name it, shown in place of the license file's */
  license?: string | null;
  sourceUrl?: string | null;
  /** Relative to the library root */
  coverPath?: string | null;
  /** The game export's origin, in place of the inferred one */
  origin?: PackOrigin | null;
  /** The game export's normal-map convention, in place of the origin's default */
  normalConvention?: NormalConvention | null;
}

/** One edit `set_pack_override` saves. An empty text field keeps what was detected. */
export type PackEdit =
  | { kind: 'mark'; mark: PackMark | null }
  | {
      kind: 'details';
      name: string | null;
      license: string | null;
      sourceUrl: string | null;
      origin: PackOrigin | null;
      normalConvention: NormalConvention | null;
    }
  | { kind: 'cover'; assetId: string | null };

/** A pack by its library and path, with its name (`get_cover_pack`) */
export interface PackRef extends FolderRef {
  name: string;
}

/** A pack's cover image, shown as a grid tile shows an asset (`tileMedia`) */
export interface Cover {
  /** Absolute */
  filePath: string;
  /** Absolute, when the image has a thumbnail */
  thumbnailPath?: string | null;
  extension: string;
  kind: AssetKind;
}

/** One card of the Packs view (`get_packs`) */
export interface PackCard {
  libraryId: string;
  /** Relative to the library root */
  path: string;
  /** Your name, else the license header's, else the folder's */
  name: string;
  /** The folder holding the pack, the card's subtitle; `''` for the library root */
  parentPath: string;
  /** Visible files below the pack; 0 shows as "No visible items" */
  itemCount: number;
  hiddenCount: number;
  cover?: Cover | null;
  /** What you said the folder is, if you did */
  mark?: PackMark | null;
}

/** The license a pack's license file or `datapackage.json` names, or `unknown` for any other */
export type LicenseKind = 'CC0' | 'CC-BY' | 'CC-BY-SA' | 'MIT' | 'Apache-2.0' | 'OFL' | 'unknown';

/**
 * A pack's license: its `datapackage.json` one, else its license file's. At most one of
 * `filePath` ("Open license") and `url` ("Open") is set.
 */
export interface PackLicense {
  kind: LicenseKind;
  /** The `datapackage.json` license's title, else its id: the label of an unknown kind */
  name?: string | null;
  /** Absolute: an indexed txt, md or pdf file */
  filePath?: string | null;
  /** An http(s) URL, opened only on a click */
  url?: string | null;
}

/** What the detail panel shows for one pack (`get_pack_detail`) */
export interface PackDetail {
  libraryId: string;
  /** Relative to the library root; `''` for a library that is itself one pack */
  path: string;
  name: string;
  /** The folder holding the pack; `''` for the library root */
  parentPath: string;
  itemCount: number;
  hiddenCount: number;
  cover?: Cover | null;
  /** The pack's `Preview*` and `Sample*` images, by path */
  gallery: Cover[];
  /** The `datapackage.json` version, else the license header's */
  version?: string | null;
  /** The `datapackage.json` publisher */
  publisher?: string | null;
  /** The `datapackage.json` authors, else the license header's author */
  authors: string[];
  /** The detected source URL, without yours */
  sourceUrl?: string | null;
  license?: PackLicense | null;
  /** Files below the pack by kind, hidden files and every format of a model included */
  kinds: Partial<Record<AssetKind, number>>;
  /** Their total size in bytes */
  sizeBytes: number;
  /** Your edits, all null when you made none */
  edits: PackOverride;
}

/**
 * The live counts beside the Types and format filters (`get_facet_counts`), each the grid's total
 * once that value is picked: the sidebar's Types replace the kind filter, and a format pill adds
 * its extension to the active ones.
 */
export interface FacetCounts {
  /** The total with each kind as the only one; kinds with none are absent */
  kinds: Record<string, number>;
  /** The total with each extension added to the active ones; read through `extensionCount` */
  extensions: Record<string, number>;
  /** The count of an extension `extensions` leaves out: the active extensions' total, or 0 */
  extensionBase: number;
}

/**
 * Live folder counts (`get_folder_counts`): for each parent path asked for, the grid's total in
 * each of its child folders, by child path. Children with none are absent.
 */
export type FolderCounts = Record<string, Record<string, number>>;

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

/** Game export settings (plan C2). Null is unset. */
export interface ExportSettings {
  /** An absolute folder outside every library */
  root: string | null;
  /** A top-level, non-smart collection whose children are the export domains */
  rootCollection: string | null;
  /** An absolute path to Blender. Null means `blender` on PATH. */
  blenderPath: string | null;
}

/** The Blender an export would run, read for Settings */
export interface BlenderStatus {
  /** The major.minor exports accept, "5.2" */
  series: string;
  path: string | null;
  /** "5.2.2" */
  version: string | null;
  /** Whether exports accept it: only Blender `series`.x */
  accepted: boolean;
  /** Why exports can't use it, when they can't */
  problem: string | null;
}

/** The fields to change, where null clears one */
export type ExportSettingsPatch = Partial<ExportSettings>;

/** A URP Lit texture role, as the game's asset contract names it */
export type TextureRole =
  | 'BaseColor'
  | 'Normal'
  | 'MetallicSmoothness'
  | 'Occlusion'
  | 'Emission'
  | 'Height';

/** An asset file an export check names */
export interface ExportAssetRef {
  libraryId: string;
  assetId: string;
  /** Relative to the library root */
  filePath: string;
}

/** Something that refuses the export, with the assets it is about */
export interface ExportProblem {
  message: string;
  assets: ExportAssetRef[];
}

/** One row of the Export view (plan C14) */
export interface ExportCheckAsset extends ExportAssetRef {
  /** `other` is a member the game can't take, such as a document */
  kind: 'model' | 'texture' | 'animation' | 'other';
  /** Its name: yours, else its default, or null when its file name gives none */
  assetName: string | null;
  role: TextureRole | null;
  /** Its pack's id, or null when it is in no pack */
  pack: string | null;
  /**
   * `ready` when the previous bundle holds it as the next one would, `changed` or `new` when it
   * doesn't, `problem` when a problem names it
   */
  status: 'ready' | 'changed' | 'new' | 'problem';
  /** In the domain itself, rather than joined through a material or its texture set */
  member: boolean;
  /** A texture's layout (plan C8): a role, or ORM, MaskMap, Roughness, Gloss or Metallic */
  layout?: string;
  /** A normal map's convention */
  normalConvention?: NormalConvention;
  /** A model's materials, as inferred (plan C7) */
  materials?: ExportMaterial[];
}

/** Where an inferred texture came from (plan D11) */
export type TextureVia = 'fbx' | 'unity' | 'suffix' | 'stem' | 'yours';

/** A material's surface (plan D23) */
export type MaterialSurface = 'opaque' | 'cutout';

/** A model's material slot, as inferred (plan C7) */
export interface ExportMaterial {
  slot: string;
  name: string;
  surface: MaterialSurface;
  /** Its base color has alpha below 0.5: a suggestion, which exports opaque until accepted */
  cutoutSuggested: boolean;
  /** The slot the export adds for a mesh with no material */
  added: boolean;
  textures: Partial<Record<TextureRole, { via: TextureVia; assetName: string; files: string[] }>>;
}

/** An asset of a manifest, as the export's diff names it */
export interface ExportDiffEntry {
  kind: 'model' | 'texture' | 'animation';
  assetName: string;
  /** A texture's role, null for a model or an animation */
  role: TextureRole | null;
}

/** What exporting changes in the bundle, by kind, name and role (plan C14) */
export interface ExportDiff {
  /** The previous bundle's `exportedAt`, null for a first export */
  previousExportedAt: string | null;
  added: ExportDiffEntry[];
  changed: ExportDiffEntry[];
  removed: ExportDiffEntry[];
  /** Ids of the packs whose manifest entry the export adds, changes or removes */
  packs: string[];
}

/** A pack of the domain, as the manifest names it (plan C6) */
export interface ExportPack {
  id: string;
  name: string;
  version?: string;
  publisher?: string;
  sourceUrl?: string;
  licence: { spdx: string | null; name: string; url?: string };
  origin: PackOrigin;
  libraryId: string;
  /** The pack folder, relative to the library root */
  path: string;
  normalConvention: NormalConvention;
}

/** What a domain's export Check found */
export interface ExportCheck {
  domain: string;
  /** Where the bundle goes, once the export root and root collection are set */
  bundlePath: string | null;
  assets: ExportCheckAsset[];
  packs: ExportPack[];
  problems: ExportProblem[];
  /** The manifest of the ready assets, as the bundle would hold it */
  manifest: unknown;
  /** The diff against the previous bundle, once the bundle path is known */
  diff: ExportDiff | null;
  /** Whether the bundle is staged, so Export can swap it in */
  staged: boolean;
}

/** A value the game export infers, and yours when you set one, which wins (plan C4) */
export interface GameField<T> {
  inferred: T;
  yours: T | null;
}

/** A texture set filling a material's role */
export interface GameTexturePick {
  /** The source the role is bound by: the image you picked, else the set's first giving it */
  assetId: string;
  /** The set's name */
  assetName: string;
  /** The set's sources that give the role */
  files: string[];
}

/** One role of a material: its texture, inferred and yours */
export interface GameRoleTexture {
  role: TextureRole;
  inferred: GameTexturePick | null;
  /** Whether you chose this role's texture, or none: `value` is then yours */
  yours: boolean;
  /**
   * The image you picked, even when the export can't use it (it left the pack, or its layout no
   * longer gives the role); null when you chose none or nothing
   */
  picked: string | null;
  /** What the export uses, null for no texture */
  value: GameTexturePick | null;
}

/** One material slot of a model, as the Game tab shows it (plan C7, C13) */
export interface GameMaterial {
  slot: string;
  /** The slot the export adds for a kept mesh with no material */
  added: boolean;
  name: GameField<string>;
  surface: GameField<MaterialSurface>;
  /** Its base color has a cut-out alpha: a suggestion you accept with one click */
  cutoutSuggested: boolean;
  /** Every role, in the contract's order */
  textures: GameRoleTexture[];
}

/** An image of a pack, with its layout and the roles it gives */
export interface GameImage {
  assetId: string;
  /** Relative to the library root */
  filePath: string;
  /** A role, or ORM, MaskMap, Roughness, Gloss or Metallic */
  layout: string;
  roles: TextureRole[];
}

/** A source image's layout as an edit names it (plan C8): `passThrough` gives one role as it is */
export type TextureLayoutEdit =
  | 'passThrough'
  | 'ORM'
  | 'MaskMap'
  | 'Roughness'
  | 'Gloss'
  | 'Metallic';

/** A texture source as the Game tab shows it (plan C13) */
export interface GameTexture {
  layout: GameField<TextureLayoutEdit>;
  /** Its role, when its layout passes one role through; other layouts fix their roles */
  role: GameField<TextureRole> | null;
  normalConvention: GameField<NormalConvention>;
  /** The sources of its texture set, itself included, by path */
  set: GameImage[];
  /** The roles its set exports */
  outputs: TextureRole[];
}

/** What the detail panel's Game tab shows for an asset (plan C13) */
export interface GameDetail {
  /** The grid item the tab is about: a group's main item */
  assetId: string;
  kind: 'model' | 'texture' | 'animation';
  assetName: GameField<string>;
  /** The domains holding it, by name */
  domains: string[];
  /** The problems naming it in its domains' Checks */
  problems: ExportProblem[];
  /** A model's materials, in slot order */
  materials: GameMaterial[];
  /** A model's pack's images, by path, which its texture pickers offer */
  images: GameImage[];
  /** A texture's own layout, role and convention, and its set */
  texture: GameTexture | null;
}

/** Your edits to one material slot: a texture is an image's id, or null for none */
export interface MaterialEdits {
  name?: string;
  surface?: MaterialSurface;
  textures?: Partial<Record<TextureRole, string | null>>;
}

/**
 * Your export edits to an asset: a value sets it, null resets it to inferred. `materials`
 * replaces each slot it names, and null resets a slot or all of them.
 */
export interface ExportEditPatch {
  assetName?: string | null;
  materials?: Record<string, MaterialEdits | null> | null;
  layout?: TextureLayoutEdit | null;
  role?: TextureRole | null;
  normalConvention?: NormalConvention | null;
}

/**
 * An `export:progress` event: how a Check's Blender jobs are going, sent once before they start
 * and once as each ends. `asset` is the file whose job just ended.
 */
export interface ExportProgress {
  domainId: string;
  done: number;
  total: number;
  asset: string | null;
}

/** What Export did: swapped the bundle in, or found that something the Check read changed */
export type ExportOutcome =
  | { status: 'exported'; bundlePath: string; assets: number }
  | { status: 'stale'; changes: string[] };

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

/**
 * What a run covers: chosen assets, or the untagged items of a library or of a folder in one
 * (every item with `allAssets`). A folder's `path` is relative to its library's root.
 */
export type AutotagScope =
  | { kind: 'assets'; assetIds: string[] }
  | { kind: 'library'; libraryId: string; allAssets: boolean }
  | { kind: 'folder'; libraryId: string; path: string; allAssets: boolean };

/** What the run dialog was opened for: chosen assets, a sidebar library, or a folder or pack */
export type AutotagTarget =
  | { kind: 'assets'; assetIds: string[] }
  | { kind: 'library'; libraryId: string; libraryName: string }
  | { kind: 'folder'; libraryId: string; path: string; name: string };

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
