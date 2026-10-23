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
