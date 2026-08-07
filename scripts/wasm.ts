// Drives the wasm-pack builds vendored into packages/*/src/wasm/generated. The
// predev/prebuild hooks, the package build scripts and CI each invoke these back
// to back, and wasm-pack rewrites its output unconditionally, so every module is
// fingerprinted: its wasm-opt pass alone costs minutes under the workspace's
// `lto = true` release profile.
import { spawnSync } from 'node:child_process';
import { createHash, type Hash } from 'node:crypto';
import { copyFile, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export interface WasmModule {
  /** Directory under crates/. */
  crate: string;
  /** wasm-bindgen artifact base name: the crate name with dashes underscored. */
  name: string;
  /** Destination for the vendored output, relative to the repo root. */
  generated: string;
  /** Flags passed through to cargo. */
  cargoArgs?: string[];
}

interface Stamp {
  key: string;
  outputs: Record<string, string>;
}

const WASM_PACK_VERSION = '0.15.0';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function requireWasmPack(): void {
  const version = spawnSync('wasm-pack', ['--version'], { encoding: 'utf8' });
  const versionErrorCode =
    version.error && 'code' in version.error && typeof version.error.code === 'string'
      ? version.error.code
      : undefined;
  if (versionErrorCode === 'ENOENT') {
    throw new Error(
      `wasm-pack ${WASM_PACK_VERSION} is required; install it with cargo install wasm-pack --version ${WASM_PACK_VERSION} --locked`
    );
  }
  if (version.status !== 0) process.exit(version.status ?? 1);
  if (version.stdout.trim() !== `wasm-pack ${WASM_PACK_VERSION}`) {
    throw new Error(`expected wasm-pack ${WASM_PACK_VERSION}, got ${version.stdout.trim()}`);
  }
}

async function hashTree(hash: Hash, dir: string, prefix: string): Promise<void> {
  const entries = await readdir(dir, { withFileTypes: true });
  entries.sort((left, right) => (left.name < right.name ? -1 : 1));
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      await hashTree(hash, path, `${prefix}${entry.name}/`);
      continue;
    }
    hash.update(`${prefix}${entry.name}\0`);
    hash.update(await readFile(path));
  }
}

/** Digest of every input the emitted wasm depends on: the crates, the workspace
 * manifest (release profiles) and these scripts. */
async function hashSources(): Promise<string> {
  const hash = createHash('sha256');
  for (const file of ['Cargo.toml', 'Cargo.lock']) {
    hash.update(`${file}\0`);
    hash.update(await readFile(resolve(root, file)));
  }
  await hashTree(hash, resolve(root, 'crates'), 'crates/');
  const scripts = resolve(root, 'scripts');
  const names = (await readdir(scripts)).filter((name) => name.includes('wasm')).sort();
  for (const name of names) {
    hash.update(`scripts/${name}\0`);
    hash.update(await readFile(join(scripts, name)));
  }
  return hash.digest('hex');
}

function vendored(name: string): string[] {
  return [`${name}.js`, `${name}.d.ts`, `${name}_bg.wasm`, `${name}_bg.wasm.d.ts`];
}

async function digest(path: string): Promise<string | null> {
  const bytes = await readFile(path).catch(() => null);
  return bytes ? createHash('sha256').update(bytes).digest('hex') : null;
}

// The stamp lives under target/ and the vendored .wasm is gitignored, so a
// target/ restored from a CI cache can outlive the output it describes; hashing
// the output too keeps the skip from serving a file that is no longer there.
async function isFresh(stamp: string, key: string, dest: string, name: string): Promise<boolean> {
  const recorded: Stamp | null = await readFile(stamp, 'utf8')
    .then((text) => JSON.parse(text))
    .catch(() => null);
  if (recorded?.key !== key) return false;
  for (const file of vendored(name)) {
    if ((await digest(resolve(dest, file))) !== recorded.outputs[file]) return false;
  }
  return true;
}

/** Builds every module whose inputs or vendored output changed. */
export async function buildWasmModules(modules: WasmModule[]): Promise<void> {
  requireWasmPack();
  const sources = await hashSources();

  for (const { crate, name, generated, cargoArgs = [] } of modules) {
    const dest = resolve(root, generated);
    const stamp = resolve(root, 'target/wasm-pack', `${crate}.json`);
    const key = createHash('sha256')
      .update(sources)
      .update(`\0${WASM_PACK_VERSION}\0${crate}\0${name}\0${cargoArgs.join(' ')}`)
      .digest('hex');

    if (await isFresh(stamp, key, dest, name)) {
      console.log(`[wasm] ${crate}: up to date`);
      continue;
    }

    const output = resolve(root, 'target/wasm-pack', crate);
    await rm(output, { recursive: true, force: true });
    // --locked must ride with the cargo pass-through: wasm-pack forwards its own
    // trailing args verbatim once a `--` section exists, and cargo rejects a
    // stray `--` marker.
    const build = spawnSync(
      'wasm-pack',
      [
        'build',
        resolve(root, 'crates', crate),
        '--release',
        '--target',
        'web',
        '--out-dir',
        output,
        ...(cargoArgs.length ? ['--', ...cargoArgs] : ['--locked']),
      ],
      { stdio: 'inherit' }
    );
    if (build.status !== 0) process.exit(build.status ?? 1);

    await mkdir(dest, { recursive: true });
    const glue = await readFile(resolve(output, `${name}.js`), 'utf8');
    const fallback = `module_or_path = new URL('${name}_bg.wasm', import.meta.url);`;
    if (!glue.includes(fallback)) throw new Error(`wasm-pack glue fallback changed (${name})`);
    await writeFile(
      resolve(dest, `${name}.js`),
      glue.replace(fallback, `throw new Error('${crate} requires an explicit module or URL');`)
    );
    for (const file of [`${name}.d.ts`, `${name}_bg.wasm`, `${name}_bg.wasm.d.ts`]) {
      await copyFile(resolve(output, file), resolve(dest, file));
    }

    const outputs: Record<string, string> = {};
    for (const file of vendored(name)) {
      outputs[file] = (await digest(resolve(dest, file))) as string;
    }
    await mkdir(dirname(stamp), { recursive: true });
    await writeFile(stamp, JSON.stringify({ key, outputs } satisfies Stamp));
  }
}
