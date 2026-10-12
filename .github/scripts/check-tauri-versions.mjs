// Fails if any @tauri-apps npm package and its Rust crate are on different
// major.minor versions, which `tauri build` refuses to build.
// Reads only the lockfiles, so it runs before any install or compile.
import { readFileSync } from 'node:fs';

const cargoLock = readFileSync('apps/desktop/src-tauri/Cargo.lock', 'utf8');
const pnpmLock = readFileSync('pnpm-lock.yaml', 'utf8');

const crates = new Map();
for (const [, name, version] of cargoLock.matchAll(/\[\[package\]\]\nname = "(tauri(?:-plugin-[a-z-]+)?)"\nversion = "([^"]+)"/g)) {
  crates.set(name, version);
}

const majorMinor = (version) => version.split('.').slice(0, 2).join('.');
let failed = false;
const seen = new Set();

// Direct dependencies in the importers section: '@tauri-apps/x': / specifier: ... / version: 1.2.3
for (const [, pkg, version] of pnpmLock.matchAll(/^\s+'@tauri-apps\/(api|plugin-[a-z-]+)':\n\s+specifier: .*\n\s+version: ([0-9][^\s(]*)/gm)) {
  if (seen.has(pkg)) continue;
  seen.add(pkg);
  const crate = pkg === 'api' ? 'tauri' : `tauri-${pkg}`;
  const crateVersion = crates.get(crate);
  if (!crateVersion) {
    console.log(`::error::@tauri-apps/${pkg} ${version} has no matching ${crate} crate in Cargo.lock`);
    failed = true;
  } else if (majorMinor(crateVersion) !== majorMinor(version)) {
    console.log(`::error::@tauri-apps/${pkg} ${version} does not match ${crate} ${crateVersion} (major.minor must be equal)`);
    failed = true;
  } else {
    console.log(`ok  @tauri-apps/${pkg} ${version} ~ ${crate} ${crateVersion}`);
  }
}

if (seen.size === 0) {
  console.log('::error::No @tauri-apps packages found in pnpm-lock.yaml; the lockfile format may have changed');
  failed = true;
}
process.exit(failed ? 1 : 0);
