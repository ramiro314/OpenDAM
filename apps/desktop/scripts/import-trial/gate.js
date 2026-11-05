// The gate's driver (docs/plans/import-material-fixes/plan.md, "Before deleting the old libraries").
// server.py loads it into the real frontend, so `modelThumbnailer` renders every model, and it
// imports each pack by clicking through the real Import pack dialog with its default steps. Only
// the native file picker is answered here. Every step is posted to the server, which appends it to
// h/log.jsonl.
(() => {
  const POLL_MS = 15000;
  // The preview counts holding still for this many polls end the wait: a render that failed this
  // session stays awaiting a render until the next launch (`modelsToRender`).
  const SETTLED_POLLS = 12;

  const real = (command, args, options) =>
    window.__TAURI_INTERNALS__.invoke(command, args, options);
  let picked = null;
  let asked = false;
  let scan = null;
  let scanError = null;
  let refused = [];

  // server.py sends every IPC call of the frontend's one call site through here.
  window.__gateInvoke = async (command, args, options) => {
    if (command === 'plugin:dialog|open') {
      asked = true;
      return picked;
    }
    if (command === 'start_import') {
      try {
        return await real(command, args, options);
      } catch (e) {
        refused.push(`${args.request.target}: ${e}`);
        throw e;
      }
    }
    if (command !== 'scan_import') return real(command, args, options);
    scan = null;
    scanError = null;
    try {
      scan = await real(command, args, options);
      return scan;
    } catch (e) {
      scanError = String(e);
      throw e;
    }
  };

  const log = (k, v) =>
    fetch('/log', { method: 'POST', body: JSON.stringify({ t: Date.now(), k, v }) });
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const $ = (selector) => document.querySelector(selector);

  async function until(test, what, seconds) {
    const end = Date.now() + seconds * 1000;
    for (;;) {
      const found = test();
      if (found) return found;
      if (Date.now() > end) throw new Error(`no ${what} after ${seconds} s`);
      await sleep(200);
    }
  }

  // React reads a control's value through its own tracker, so go round it with the native setter.
  function setValue(control, value) {
    const prototype = Object.getPrototypeOf(control);
    Object.getOwnPropertyDescriptor(prototype, 'value').set.call(control, value);
    const kind = control instanceof HTMLSelectElement ? 'change' : 'input';
    control.dispatchEvent(new Event(kind, { bubbles: true }));
  }

  async function scanned() {
    await until(() => scan || scanError, 'scan', 1800);
    if (scanError) throw new Error(scanError);
    return until(() => $('#import-publisher'), 'draft', 60);
  }

  const brief = (candidate) => ({
    name: candidate.name,
    target: candidate.target,
    ticked: candidate.ticked,
    preset: candidate.preset,
    steps: candidate.steps.map((step) => step.kind),
    files: candidate.tree?.files ?? null,
    glbs: candidate.tree?.glbs ?? null,
    estimate: candidate.tree?.estimate ?? false,
    warnings: candidate.tree?.warnings ?? [],
    notes: candidate.tree?.notes ?? [],
    treeError: candidate.tree?.error ?? null,
    skipped: candidate.tree?.skipped ?? [],
  });

  async function importPack(pack) {
    picked = pack.path;
    asked = false;
    scan = null;
    scanError = null;
    // A click before the app has listed its libraries only says to add one, so click until asked.
    for (let tries = 0; !asked; tries++) {
      if (tries === 60) throw new Error('Import pack never asked for a file');
      $('button[title^="Bring a .unitypackage"]').click();
      await sleep(1000);
    }
    await scanned();

    const library = $('#import-dialog select');
    if (library.value !== pack.lib) {
      scan = null;
      setValue(library, pack.lib);
      await scanned();
    }

    const radios = [...document.getElementsByName('import-offer')];
    const opened = Math.max(
      radios.findIndex((radio) => radio.checked),
      0
    );
    const layouts = scan.offers.map((offer) => offer.layout);
    const hint = [...document.querySelectorAll('#import-dialog span')]
      .map((span) => span.textContent)
      .find((text) => / of \d+ ticked$/.test(text));
    await log('dialog', { path: pack.path, layouts, openedOn: layouts[opened], hint });

    const offer = layouts.indexOf(pack.offer);
    if (offer < 0) throw new Error(`no ${pack.offer} offer among ${layouts}`);
    if (offer !== opened) radios[offer].click();

    if (pack.publisher) setValue($('#import-publisher'), pack.publisher);

    const submit = await until(
      () => {
        const button = $('#import-submit');
        return button && !button.disabled ? button : null;
      },
      'enabled Import',
      1800
    );
    const candidates = scan.offers[offer].candidates;
    await log('tree', { path: pack.path, candidates: candidates.map(brief) });
    await log('submit', { path: pack.path, label: submit.textContent });
    refused = [];
    submit.click();
    // A refused pack leaves the dialog open, so stop waiting once Import is done with its packs.
    await until(
      () =>
        !$('#import-dialog') ||
        (refused.length > 0 && $('#import-submit')?.textContent !== 'Importing…'),
      'closed dialog',
      3600
    );
    if (refused.length > 0) throw new Error(`refused: ${refused.join('; ')}`);
    return candidates.filter((candidate) => candidate.ticked).length;
  }

  function closeDialog() {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
  }

  async function listRuns(libraries, since) {
    const lists = await Promise.all(
      libraries.map((libraryId) => real('list_import_runs', { libraryId }))
    );
    return lists.flat().filter((run) => run.createdAt >= since);
  }

  async function waitForQueue(libraries, since) {
    let last = '';
    for (;;) {
      const runs = await listRuns(libraries, since);
      const counts = {};
      for (const run of runs) counts[run.status] = (counts[run.status] ?? 0) + 1;
      const running = runs.find((run) => run.status === 'running');
      const snapshot = JSON.stringify([counts, running?.id, running?.phase]);
      if (snapshot !== last) await log('runs', { counts, phase: running?.phase });
      last = snapshot;
      if (!runs.some((run) => ['queued', 'running'].includes(run.status))) return runs;
      await sleep(POLL_MS);
    }
  }

  async function waitForPreviews(minutes) {
    const end = Date.now() + minutes * 60000;
    let last = '';
    let still = 0;
    for (;;) {
      const progress = await real('get_preview_progress', {});
      const snapshot = JSON.stringify(progress);
      still = snapshot === last ? still + 1 : 0;
      if (snapshot !== last) await log('previews', progress);
      last = snapshot;
      // Two equal polls at least, as a run's last pass may not have queued its previews yet.
      const idle = progress.modelsAwaitingRender === 0 ? still >= 1 : still >= SETTLED_POLLS;
      if (progress.finished === progress.total && idle) {
        return { ...progress, settled: true };
      }
      if (Date.now() > end) return { ...progress, settled: false };
      await sleep(POLL_MS);
    }
  }

  async function drive() {
    const { imports, settleMinutes = 60 } = await (await fetch('/packs.json')).json();
    await log('start', imports.length);
    await until(() => $('button[title^="Bring a .unitypackage"]'), 'Import pack button', 300);
    const since = Date.now();
    let errors = 0;
    let expected = 0;
    for (const pack of imports) {
      try {
        expected += await importPack(pack);
      } catch (e) {
        errors += 1;
        await log('start_err', { path: pack.path, e: String(e) });
        closeDialog();
      }
    }
    const libraries = [...new Set(imports.map((pack) => pack.lib))];
    const runs = await waitForQueue(libraries, since);
    await log('runs_final', runs);
    await log('previews_final', await waitForPreviews(settleMinutes));
    const failed = runs.filter((run) => run.status !== 'done');
    await log('result', {
      runs: runs.length,
      errors,
      failed: failed.map((run) => [run.packs, run.status, run.error]),
    });
    await log('done', errors === 0 && failed.length === 0 && runs.length === expected);
  }

  window.addEventListener('load', () => {
    drive().catch((e) => log('done', String(e)));
  });
})();
