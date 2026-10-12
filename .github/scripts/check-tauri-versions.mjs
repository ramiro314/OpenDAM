// Fails if any @tauri-apps npm package and its Rust crate are on different
// major.minor versions, which `tauri build` refuses to build.
// Reads only the lockfiles, so it runs before any install or compile.
import { readFileSync } from 'node:fs';

const cargoLock = readFileSync('apps/desktop/src-tauri/Cargo.lock', 'utf8');
const bunLock = readFileSync('bun.lock', 'utf8');

const crates = new Map();
for (const [, name, version] of cargoLock.matchAll(/\[\[package\]\]\nname = "(tauri(?:-plugin-[a-z-]+)?)"\nversion = "([^"]+)"/g)) {
  crates.set(name, version);
}

const majorMinor = (version) => version.split('.').slice(0, 2).join('.');
let failed = false;
const seen = new Set();

// Resolved packages in the "packages" section: "@tauri-apps/x": ["@tauri-apps/x@1.2.3", ...
for (const [, pkg, version] of bunLock.matchAll(/^\s+"@tauri-apps\/(api|plugin-[a-z-]+)": \["@tauri-apps\/\1@([0-9][^"]*)"/gm)) {
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
  console.log('::error::No @tauri-apps packages found in bun.lock; the lockfile format may have changed');
  failed = true;
}
process.exit(failed ? 1 : 0);
