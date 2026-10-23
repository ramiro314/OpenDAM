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
