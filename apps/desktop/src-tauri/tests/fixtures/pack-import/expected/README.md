# Pack import goldens

Expected outputs for the pack import's tests (docs/plans/pack-import/plan.md, Fixtures and
goldens). They come from running the owner's scripts in `~/Documents/Assets` once on the fixtures
beside this folder. The scripts themselves are not copied into the repo (plan D30).

## `synthetic.extract.txt`

The files `extract_unity.py` writes for `synthetic.unitypackage`, relative to the destination and
sorted bytewise, one per line (A2).

- Script: `extract_unity.py`, sha256
  `f81fdddb787ece47efe630ab6323bd1134f769a293589d4cd7ca499acf40b9c4`
- Command, from `tests/fixtures/pack-import/`:

  ```sh
  python3 -I ~/Documents/Assets/extract_unity.py --package synthetic.unitypackage "$OUT/synthetic" --keep-all
  (cd "$OUT/synthetic" && find . -type f | sed 's|^\./||' | LC_ALL=C sort) > expected/synthetic.extract.txt
  ```

`escape.unitypackage` has no golden: the script and the port differ there by design (plan R13).

## `synthetic.place.txt`

The files left after `flatten_pack.py --hoist Art` runs over that extraction, in the same form
(A3): the wrapper folders `Built-In/Built-In_Pack/Pack/` merged into the pack root, then `Art/`.

- Script: `flatten_pack.py`, sha256
  `604b637a8cf1207455475b46a5cbb572eafe31a6527b7b112480817726b3b099`
- Command, after the one above:

  ```sh
  python3 -I ~/Documents/Assets/flatten_pack.py "$OUT/synthetic" --hoist Art
  (cd "$OUT/synthetic" && find . -type f | sed 's|^\./||' | LC_ALL=C sort) > expected/synthetic.place.txt
  ```

## `*.datapackage.json`

The `datapackage.json` files `gen_datapackage.py` and `gen_kenney_datapackage.py` write for the
Unity, Fab and Kenney fixtures, byte for byte (A7). Each pack's tree is first made as the import's
preset makes it.

- Scripts: `gen_datapackage.py`, sha256
  `8c59da9f2cc027c890042bc80175ef569b280c60254957398669bdff70a64b92`, and
  `gen_kenney_datapackage.py`, sha256
  `d90b42a233863575cc4c2d932da7220be3fd0413f8a870c08c46dd3ce57c0140`
- Goldens, by sha256:
  - `synthetic.datapackage.json`
    `ac60a969a7ed5ec9ab73351ee623b45ba063559acca11e17a5b628d8b027d660`
  - `fab-unity.datapackage.json`
    `8b304623575bde03a84d204987c0655d4b0aa4154dcc3254300debc4042f5b45`
  - `kenney-car-kit.datapackage.json`
    `3adbe9ccba7dc1a42308b56528cb24de72c5dc9b5ff6d1ec56d810ee8cfdfad3`
  - `kenney-tiles.datapackage.json`
    `ccf66c93249f24f5f412da0e0b377afbbd9a183266c8eae4dab98d288dfa41bf`
- Commands, from `tests/fixtures/pack-import/`, with `A=~/Documents/Assets` and an empty `$OUT`:

  ```sh
  # synthetic.datapackage.json: the Unity preset (extract, flatten), publisher Synthetic Studio
  python3 -I $A/extract_unity.py --package synthetic.unitypackage "$OUT/Unity/Synthetic Studio/Synthetic" --keep-all
  python3 -I $A/flatten_pack.py "$OUT/Unity/Synthetic Studio/Synthetic"
  python3 -I $A/gen_datapackage.py "$OUT/Unity"
  cp "$OUT/Unity/Synthetic Studio/Synthetic/datapackage.json" expected/synthetic.datapackage.json

  # fab-unity.datapackage.json: the Fab preset (the URP variant, flatten, hoist Art)
  (mkdir -p "$OUT/zip" && cd "$OUT/zip" && unzip -q "$OLDPWD/fab-unity.zip" && unzip -q "$OLDPWD/kenney.zip")
  python3 -I $A/extract_unity.py --package "$OUT/zip/Crypt/Crypt_URP.unitypackage" "$OUT/Fab/Synthetic Studio/Crypt" --keep-all
  python3 -I $A/flatten_pack.py "$OUT/Fab/Synthetic Studio/Crypt" --hoist Art
  python3 -I $A/gen_datapackage.py --publisher "$OUT/Fab/Synthetic Studio" --license fab
  cp "$OUT/Fab/Synthetic Studio/Crypt/datapackage.json" expected/fab-unity.datapackage.json

  # kenney-*.datapackage.json: the Kenney preset (the pack's folder as it is in the zip)
  unshare -rn python3 -I $A/gen_kenney_datapackage.py "$OUT/zip/Kenney"
  cp "$OUT/zip/Kenney/3D assets/Fixture Car Kit/datapackage.json" expected/kenney-car-kit.datapackage.json
  cp "$OUT/zip/Kenney/2D assets/Fixture Tiles/datapackage.json" expected/kenney-tiles.datapackage.json
  ```

  The Kenney script runs without a network (`unshare -rn`), so its HEAD requests to kenney.nl
  fail and it writes no `sources`, as it would anyway: the fixtures' titles have no store page.
  The lookup's tests inject the HTTP answer instead.
