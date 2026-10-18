# Fast browsing for big collections: final run results

Status: **incomplete.** This is step 8 of the [plan](plan.md) ("Suggested build order"). The
`gpu-shm` timing run was interrupted when the PC hung on 2026-10-18 (see
[The hang](#the-hang-on-2026-10-18)). The owner then asked for no further bench rounds, so:

- the `cpu` and `explicit-sync` runs were not made;
- U1-U8 were not run in `checks` mode.

This record keeps what was measured, so the next attempt starts from it. Two of the ticket's
acceptance criteria are therefore **not met**, by the owner's decision rather than by a measured
failure:

- `cpu` and `explicit-sync` runs beside `gpu-shm`, and so the explicit-sync 15% test;
- U1-U8 passing in `checks` mode on the final state, with screenshots.

Meeting them needs a rerun the owner approves. GPU-mode runs risk the same hang on this machine.

## Setup

- **Machine:** RTX 5080, NVIDIA driver 615.78.08, WebKitGTK 2.54.1.
- **Where:** the scroll bench's nested headless KWin, 1400x900, `devicePixelRatio` 1.
- **Data:** fresh run copies of the golden Kenney snapshot, 83,095 assets.
- **Run 1, on `main` at 6a0ec6e (everything merged):**
  - All nine scenarios started. lg/endToEnd timed out (below).
  - The one-time thumbnail cleanup pass ran in the background of every scenario. Each scenario
    starts a fresh app process, and the pass started over in each one. So the scenarios ran beside
    its reads of 58k image headers, on the upscaled thumbnails it had not cleared yet.
  - lg/endToEnd reached the pass's clearing phase, which holds the library's connections for
    about 16 minutes, and was stopped by the 30-minute launch timeout.
- **Run 2, on 6302416:**
  - 6302416 adds a warm-up to `scroll-bench.sh`, which runs the pass to the end before any
    scenario.
  - The warm-up finished: 56,698 thumbnails cleared out of 58,147 checked.
  - sm/continuousScroll and sm/endToEnd completed. The PC hung during sm/randomJumps.

## Targets at md, `gpu-shm`

From run 1, with the cleanup pass running in the background, which is worse than steady state.
Run 2 never reached md.

| # | Target | Measured | Result |
|---|---|---|---|
| T1 | DOM nodes max/min ≤ 1.10, end to end | 759 / 549 = 1.38 | FAIL |
| T2 | Jump to filled tiles ≤ 300 ms at p95 | p95 144 ms (p50 122, max 145), 0 of 20 timed out | PASS |
| T3 | Continuous scroll p95 ≤ 20 ms, no frame over 100 ms | p95 18 ms, max 21 ms, 0 over 100 ms | PASS |
| T4 | Web-process RSS grows < 150 MB end to end | 370.6 → 1,385.8 MB (+1,015 MB, max 1,478 MB) | FAIL |

`cpu` and `explicit-sync` runs of the same scenarios: **not measured.**

## Comparison with the baseline

The baseline is [`baseline-cpu.json`](baseline-cpu.json): the grid before windowing, in `cpu` mode,
on the same snapshot. The baseline's end to end is a different scenario: it jumps to the bottom
until 120 s pass, and only reached 9,400 tiles. That makes its RSS and DOM figures understate the
old grid's cost.

| md | Baseline (`cpu`, old grid) | Now (`gpu-shm`, run 1) |
|---|---|---|
| Continuous scroll frame interval p50 / p95 / max | 17 / 18 / 197 ms | 17 / 18 / 21 ms |
| Continuous scroll frames over 50 / 100 ms | 18 / 14 | 0 / 0 |
| Continuous scroll DOM nodes | 1,790 → 24,198 | 578-719 |
| Continuous scroll RSS growth | +322 MB | +374 MB |
| End to end DOM nodes | 1,790 → 132,090 (9,400 tiles) | 549-759 (all 83,095) |
| End to end RSS growth | +1,840 MB (9,400 tiles) | +1,015 MB (all 83,095) |
| End to end frame p95 / max | 1,219 / 1,372 ms | 22 / 87 ms |
| Random jumps | not possible | p95 144 ms |

The sm runs show how much of the end-to-end RSS growth the cleanup pass caused:

| sm, `gpu-shm` | Run 1 (pass in background) | Run 2 (pass finished first) |
|---|---|---|
| End to end RSS | 428.1 → 1,923.4 MB (+1,495) | 369.8 → 1,092.3 MB (+722) |
| End to end frame p95 / max | 25 / 79 ms | 24 / 88 ms |
| Continuous scroll frame p95 / max | 18 / 34 ms | 18 / 22 ms |
| Continuous scroll RSS growth | +586 MB | +462 MB |

## The explicit-sync switch

**Can't be decided:** there is no `explicit-sync` run, so the plan's test can't be applied (md
continuous-scroll p95 at least 15% better than `gpu-shm`).

`gpu-shm`'s md p95 is 18 ms with no frame over 21 ms. That is about the 16.7 ms frame interval of
the nested KWin's 60 Hz output. If `explicit-sync` is also held to that pace, it can't be 15% better
than 18 ms, which would mean about 15.3 ms.

**Recommendation:** keep `gpu-shm` as the default. Switching would also need the owner's 10-minute
flicker check.

## UI checks U1-U8

**Not run** on this final state. Earlier tickets ran them:

- **#97:** U1-U5 and U8 passed in one run, peaking at 779 MB.
- **#96:** U6, U7 and U8 passed in a run of their own.

No full U1-U8 run on `main` with everything merged has been recorded.

## The hang on 2026-10-18

From the previous boot's journal:

1. **11:56:33:** the sm/randomJumps app started and never received its first page. The two
   scenarios before it finished normally. Bench app scopes peaked at 0.9-1.1 GB, far under the
   10 GB cap.
2. **11:59:35-12:00:21:** the monitor (DP-1) disconnected and reconnected. Plasma logged "There
   are no outputs".
3. **From about 12:02:** the desktop's own KWin (PID 14716, running since Oct 10, not the nested
   one) was stuck inside the NVIDIA driver.
   - It was waiting on the GPU's GSP firmware (`kgspRecvPoll`, `_issueRpcAndWait`), which had
     stopped answering.
   - The `nvidia-modeset` kernel thread was blocked behind it from 12:02, and the display froze.
   - The owner pressed the power key at 12:18:43.

There was no OOM kill during the bench scenarios. The bench was the main GPU user at the time: the
nested KWin plus WebKit's GPU painting with shared-memory readback (`gpu-shm`). Its app stalling
three minutes before the monitor event fits the GPU already misbehaving. This does not prove the
bench caused the firmware stall, but it is the likely contributor. Rerunning the GPU modes on this
machine risks the same hang.

A closer look is in the [hang report](../../reports/gpu-hang-2026-10-18/report.html). KWin was
applying the reconnected monitor's mode, and the driver's VRR-off DisplayPort AUX write went to the
GPU firmware, which never replied. Two freshly started GPU programs had already failed before that:
the bench app at 11:56:33 and KSplash at 11:59:58. So the firmware probably stopped answering when
the bench killed one GPU-rendering app and started the next.

Separately, at 11:30:48, during the bench script's cargo and Vite build, which runs outside the
memory cap, the machine hit a global OOM. The kernel killed a 1.7 GB `node` process in another
Mission Control session.

## Known gaps

- **T1 misses** (1.38 against 1.10).
  - The DOM stays between 549 and 759 nodes over all 83,095 assets, instead of growing with
    scroll depth.
  - The spread between top, middle and bottom is wider than the target allows.
  - Likely cause, not investigated: rows only partly filled near the end, and placeholder tiles
    having fewer nodes than full cards.
- **T4 misses** (+1,015 MB at md with the pass running, +722 MB at sm without it).
  - Likely cause, not investigated: WebKit keeping decoded images and GPU tiles for the thousands
    of tiles visited, which a 150 MB budget can't hold.
  - Not a small fix inside the plan's design.
- **No `cpu` or `explicit-sync` runs,** so the explicit-sync recommendation rests on the
  frame-pacing argument above, not on a measurement.
- **U1-U8 not run** on the final state, so there are no screenshots.
- **md was measured only with the cleanup pass running.** A clean md run needs 6302416's warm-up.
- **The bench build runs outside the memory cap,** and at 11:30:48 it coincided with a global OOM.

## Reports

Raw reports are in `apps/desktop/src-tauri/target/scroll-bench/` (gitignored):

| Report | Scenario | Run |
|---|---|---|
| `gpu-shm-1792346037.json` | md continuous scroll | 1 |
| `gpu-shm-1792346100.json` | md end to end | 1 |
| `gpu-shm-1792346136.json` | md random jumps | 1 |
| `gpu-shm-1792345847.json` | sm continuous scroll | 1 |
| `gpu-shm-1792345909.json` | sm end to end | 1 |
| `gpu-shm-1792349759.json` | sm continuous scroll | 2 |
| `gpu-shm-1792349791.json` | sm end to end | 2 |
