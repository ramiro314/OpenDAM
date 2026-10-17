/** Extensions the webview displays itself, so the original can stand in for a thumbnail. */
const WEBVIEW_DISPLAYABLE = new Set(['png', 'jpg', 'jpeg', 'webp', 'bmp', 'gif', 'svg']);

/** Whether the webview can display a file with extension `ext` (case-insensitive, no dot). */
export function isWebviewDisplayable(ext: string): boolean {
  return WEBVIEW_DISPLAYABLE.has(ext.toLowerCase());
}
