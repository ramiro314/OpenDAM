import { useEffect } from 'react';
import { useAssetStore, usePacksStore } from '../store';
import { browseView, packsScope, scopeKey } from '../utils/packsView';

/**
 * What the main area shows for the current query (`browseView`), loading the selected node's
 * packs when it changes, and whether the node has packs below it (null while they load), which
 * shows the toolbar's Packs/Assets toggle.
 */
export function useBrowseView() {
  const query = useAssetStore((s) => s.query);
  const mode = usePacksStore((s) => s.mode);
  const loadedKey = usePacksStore((s) => s.key);
  const count = usePacksStore((s) => s.packs?.length ?? 0);
  const load = usePacksStore((s) => s.load);
  const scope = packsScope(query);
  const key = scope && scopeKey(scope);

  useEffect(() => {
    void load(scope);
    // `key` stands for `scope`
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps

  const hasPacks = key === null ? false : loadedKey === key ? count > 0 : null;
  return { view: browseView(hasPacks, query, mode), hasPacks };
}
