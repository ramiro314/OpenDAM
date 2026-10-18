# Fast browsing for big collections: final run results

Status: **partly complete.** This is step 8 of the [plan](plan.md) ("Suggested build order").

| Acceptance criterion | State |
|---|---|
| T1-T4 at md in `gpu-shm` | Measured on a clean run: T2 and T3 PASS, T1 and T4 FAIL |
| `explicit-sync` runs of the same scenarios | 8 of 9 scenarios measured, including every md scenario. lg end to end stalled at launch (below). |
| `cpu` runs of the same scenarios | All 9 scenarios measured (2d130b7, the same code as 0e79743) |
| U1-U8 in `checks` mode, with screenshots | **Not run** |
| The explicit-sync 15% test | Applied: 0% better, **fails** |
| The default stays `gpu-shm` | Yes. No render-mode code changed. |

The checks wait for the owner. A GPU-mode launch on this machine has twice stalled at startup, and
once that froze the desktop ([The hang](#the-hang-on-2026-10-18)).

## Setup

- **Machine:** RTX 5080, NVIDIA driver 615.78.08, WebKitGTK 2.54.1.
- **Where:** the scroll bench's nested headless KWin, 1400x900, `devicePixelRatio` 1.
- **Data:** a fresh run copy of the golden Kenney snapshot for each run, 83,095 assets.
- **Code:** `main` at 6a0ec6e (everything merged), plus two bench-script fixes from this ticket:
  - 6302416 runs the one-time thumbnail cleanup pass to the end before any scenario. Without it,
    every scenario's fresh app process restarted the pass. The scenarios then ran beside its reads
    of 58k image headers, on thumbnails it had not cleared yet.
  - 0e79743 waits 10 s after each app exits before starting the next.
- **The final runs** used a clean tree: `gpu-shm` and `explicit-sync` on 0e79743, and `cpu` on
  2d130b7, which only adds this record. Each warm-up cleared 56,698 thumbnails out of
  58,147 checked.

## Targets at md

| # | Target | `gpu-shm` | `explicit-sync` | `cpu` |
|---|---|---|---|---|
| T1 | DOM nodes max/min ≤ 1.10, end to end | 759 / 549 = 1.38, **FAIL** | 759 / 549 = 1.38, **FAIL** | 759 / 549 = 1.38, **FAIL** |
| T2 | Jump to filled tiles ≤ 300 ms at p95 | p95 123 ms (p50 121, max 129), 0 of 20 timed out, **PASS** | p95 122 ms (p50 122, max 122), 0 of 20 timed out, **PASS** | p95 138 ms (p50 117, max 143), 0 of 20 timed out, **PASS** |
| T3 | Continuous scroll p95 ≤ 20 ms, no frame over 100 ms | p95 18 ms, max 21 ms, 0 over 100 ms, **PASS** | p95 18 ms, max 21 ms, 0 over 100 ms, **PASS** | p95 18 ms, max 20 ms, 0 over 100 ms, **PASS** |
| T4 | Web-process RSS grows < 150 MB end to end | 539.0 → 1,249.6 MB (+711), **FAIL** | 523.7 → 1,298.7 MB (+775), **FAIL** | 513.8 → 822.7 MB (+309), **FAIL** |

The GPU modes grow about 400 MB more than `cpu` over the same end-to-end scroll. Frame times are
the same in all three, held to the nested KWin's 60 Hz output.

All sizes, for reference:

| | Continuous p95 / max (`gpu-shm`, `explicit-sync`, `cpu`) | End to end RSS growth (`gpu-shm`, `explicit-sync`, `cpu`) | Jumps p95 (`gpu-shm`, `explicit-sync`, `cpu`) |
|---|---|---|---|
| sm | 18 / 29, 18 / 23, 18 / 21 ms | +540, +909, +710 MB | 139, 138, 153 ms |
| md | 18 / 21, 18 / 21, 18 / 20 ms | +711, +775, +309 MB | 123, 122, 138 ms |
| lg | 18 / 20, 18 / 21, 18 / 21 ms | +345, stalled at launch, +284 MB | 122, 122, 116 ms |

## Comparison with the baseline

The baseline is [`baseline-cpu.json`](baseline-cpu.json): the grid before windowing, in `cpu` mode,
on the same snapshot. Its end to end is a different scenario: it jumps to the bottom until 120 s
pass, and only reached 9,400 tiles. So its RSS and DOM figures understate the old grid's cost.

| md | Baseline (`cpu`, old grid) | Now, `cpu` (same mode) | Now, `gpu-shm` |
|---|---|---|---|
| Continuous scroll frame interval p50 / p95 / max | 17 / 18 / 197 ms | 17 / 18 / 20 ms | 17 / 18 / 21 ms |
| Continuous scroll frames over 50 / 100 ms | 18 / 14 | 0 / 0 | 0 / 0 |
| Continuous scroll DOM nodes | 1,790 → 24,198 | 578-719 | 578-719 |
| Continuous scroll RSS growth | +322 MB | +181 MB | +335 MB |
| End to end DOM nodes | 1,790 → 132,090 (9,400 tiles) | 549-759 (all 83,095) | 549-759 (all 83,095) |
| End to end RSS growth | +1,840 MB (9,400 tiles) | +309 MB (all 83,095) | +711 MB (all 83,095) |
| End to end frame p95 / max | 1,219 / 1,372 ms | 18 / 58 ms | 21 / 47 ms |
| Random jumps | not possible | p95 138 ms | p95 123 ms |

In the same `cpu` mode, the windowed grid scrolls all 83,095 assets with a sixth of the memory
growth the old grid needed for 9,400.

Without the cleanup pass running, md end to end grows +711 MB, against +1,015 MB in an earlier run
on 6a0ec6e with the pass running.

## The explicit-sync switch

The plan's test is whether `explicit-sync`'s md continuous-scroll p95 frame interval is at least
15% better than `gpu-shm`'s. Both are 18 ms, so it is **0% better and fails**.

Both modes are held to the nested KWin's 60 Hz output: a 16.7 ms frame interval, so 17 ms at p50.
Neither shows a frame over 21 ms at md. On this harness the two are indistinguishable.

**Recommendation:** keep `gpu-shm` as the default. The owner's 10-minute flicker check is not needed,
since the measurement already rules the switch out.

## UI checks U1-U8

**Not run** on this final state. Earlier tickets ran them:

- **#97:** U1-U5 and U8 passed in one run, peaking at 779 MB.
- **#96:** U6, U7 and U8 passed in a run of their own.

No full U1-U8 run on `main` with everything merged has been recorded.

## Launch stalls in GPU modes

A freshly started bench app has stalled at startup twice in about 30 GPU-mode launches. Each time it
logged "indexing complete" and never received its first page.

1. **`gpu-shm`, 11:56:33.** This one froze the desktop (below).
2. **`explicit-sync`, lg end to end, 14:06:10.**
   - The app used only 291 MB, the kernel logged nothing, and the desktop kept working.
   - The bench's 30-minute launch timeout killed it at 14:36.
   - The next launch, lg random jumps at 14:36:25, worked normally. So killing the stuck app freed
     the GPU again.
   - The 10 s pause between launches (0e79743) did not prevent it.

## The hang on 2026-10-18

From the previous boot's journal, and in more detail in the
[hang report](../../reports/gpu-hang-2026-10-18/report.html):

1. **11:56:33:** the `gpu-shm` sm random-jumps app started and stalled (stall 1 above). Bench app
   scopes peaked at 0.9-1.1 GB, far under the 10 GB cap.
2. **11:59:35-12:00:21:** the monitor (DP-1) disconnected and reconnected.
3. **From about 12:02:** the desktop's own KWin (PID 14716, not the nested one) was stuck in the
   NVIDIA driver.
   - It was applying the reconnected monitor's mode.
   - The driver's VRR-off DisplayPort AUX write went to the GPU's GSP firmware, which never replied.
   - The `nvidia-modeset` kernel thread was blocked behind KWin, and the display froze.
   - The owner pressed the power key at 12:18:43.

There was no OOM kill during the scenarios. KSplash, started at 11:59:58, also never came up.

The second stall, at 14:06, ended when its app was killed, so the firmware wasn't dead for good.
The pattern fits one app getting stuck in a GPU call at startup. If the desktop needs the display
then, as with a monitor reconnect, the whole session freezes. That is a reading, not a proven cause.

Separately, at 11:30:48, during the bench script's cargo and Vite build, which runs outside the
memory cap, the machine hit a global OOM. The kernel killed a 1.7 GB `node` process in another
Mission Control session.

## Known gaps

- **T1 misses** (1.38 against 1.10, both GPU modes).
  - The DOM stays between 549 and 759 nodes over all 83,095 assets, instead of growing with
    scroll depth.
  - The spread between top, middle and bottom is wider than the target allows.
  - Likely cause, not investigated: rows only partly filled near the end, and placeholder tiles
    having fewer nodes than full cards.
- **T4 misses** at md: +711 MB in `gpu-shm`, +775 MB in `explicit-sync`, +309 MB in `cpu`.
  - Likely cause, not investigated: WebKit keeping decoded images for the thousands of tiles
    visited, which a 150 MB budget can't hold. The GPU modes add about 400 MB more, probably GPU
    tile or texture memory.
  - Not a small fix inside the plan's design.
- **U1-U8 not run** on the final state, so there are no screenshots.
- **`explicit-sync` lg end to end not measured,** because its launch stalled.
- **GPU-mode launch stalls,** cause not established (above). They risk freezing the desktop.
- **The bench build runs outside the memory cap,** and at 11:30:48 it coincided with a global OOM.

## Reports

Raw reports are in `apps/desktop/src-tauri/target/scroll-bench/` (gitignored):

| Report | Contents |
|---|---|
| `gpu-shm-1792355648.json` | Final `gpu-shm` run on 0e79743, all nine scenarios merged |
| `cpu-1792362604.json` | Final `cpu` run on 2d130b7, all nine scenarios merged |
| `explicit-sync-1792357296.json`, `-357339`, `-357356` | `explicit-sync` sm continuous scroll, end to end, random jumps |
| `explicit-sync-1792357428.json`, `-357470`, `-357488` | `explicit-sync` md continuous scroll, end to end, random jumps |
| `explicit-sync-1792357559.json`, `-359392` | `explicit-sync` lg continuous scroll, random jumps |
| `gpu-shm-1792346037.json`, `-346100`, `-346136` | Earlier `gpu-shm` md run on 6a0ec6e, with the cleanup pass running |
