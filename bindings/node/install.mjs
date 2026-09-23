// Build Skarve from the included Rust source; registry archives carry no core binary.
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readdirSync, rmSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('.', import.meta.url));
if (process.platform !== 'linux' || process.arch !== 'x64') {
  throw new Error('Skarve source installation currently supports Linux x86-64 only');
}
const sourceParent = join(root, 'native_source');
const sourceNames = existsSync(sourceParent)
  ? readdirSync(sourceParent).filter(name => name.startsWith('skarve-') && existsSync(join(sourceParent, name, 'Cargo.toml')))
  : [];
if (sourceNames.length !== 1) throw new Error('Skarve source package is incomplete: missing Rust crate');
const source = join(sourceParent, sourceNames[0]);
const env = { ...process.env, PATH: `${join(homedir(), '.cargo/bin')}:${process.env.PATH ?? ''}`, CARGO_BUILD_JOBS: '2' };
function requireTool(command, args, help) {
  const result = spawnSync(command, args, { env, stdio: 'ignore' });
  if (result.status !== 0) throw new Error(help);
}
requireTool('cargo', ['--version'], 'Skarve needs Rust 1.98.1 to build from source; install rustup first');
requireTool('gdal-config', ['--version'], 'Skarve needs GDAL development headers; install libgdal-dev first');
requireTool('pkg-config', ['--modversion', 'libdeflate'], 'Skarve needs pkg-config and libdeflate-dev');

const target = mkdtempSync(join(tmpdir(), 'skarve-node-native-'));
try {
  env.CARGO_TARGET_DIR = target;
  env.RUSTFLAGS = `--remap-path-prefix=${source}=skarve`;
  const result = spawnSync('cargo', ['build', '--locked', '--release', '--lib', '-j', '2'],
    { cwd: source, env, stdio: 'inherit' });
  if (result.status !== 0) throw new Error(`Skarve Rust build failed (exit ${result.status ?? 'signal'}). Check Rust, GDAL and libdeflate development packages.`);
  const built = join(target, 'release', 'libraster_engine.so');
  if (!existsSync(built)) throw new Error('Cargo did not produce the Skarve native library');
  const destination = join(root, 'native', 'linux-x64');
  mkdirSync(destination, { recursive: true });
  copyFileSync(built, join(destination, 'libraster_engine.so'));
} finally {
  rmSync(target, { recursive: true, force: true });
}
