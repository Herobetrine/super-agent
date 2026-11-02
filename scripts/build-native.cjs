'use strict';
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const env = { ...process.env };
const cargoBin = path.join(env.CARGO_HOME || path.join(os.homedir(), '.cargo'), 'bin');
env.PATH = [env.PATH, cargoBin].join(path.delimiter);
const rust = spawnSync('rustc', ['-vV'], { env, encoding: 'utf8' });
if (rust.status !== 0) throw new Error('请安装 Rust stable（含 cargo）后重试');
if (process.platform === 'win32' && rust.stdout.includes('host: x86_64-pc-windows-gnu')) {
  // Rust's standalone GNU distribution may ship dlltool without an assembler.
  // Prefer an explicitly installed LLVM dlltool while keeping the GNU runtime.
  const toolRoot = path.join(env.LOCALAPPDATA || os.homedir(), 'SuperAgent', 'toolchain');
  const candidates = fs.existsSync(toolRoot)
    ? fs
        .readdirSync(toolRoot)
        .filter((v) => v.startsWith('llvm-mingw-'))
        .sort()
        .reverse()
        .map((dir) => path.join(toolRoot, dir, 'bin', 'dlltool.exe'))
    : [];
  const dlltool = env.SUPERAGENT_DLLTOOL || candidates.find((p) => fs.existsSync(p));
  if (dlltool) {
    if (!fs.existsSync(dlltool)) throw new Error('找不到 SUPERAGENT_DLLTOOL');
    env.PATH = path.dirname(dlltool) + ';' + env.PATH;
    const sysroot = spawnSync('rustc', ['--print', 'sysroot'], { env, encoding: 'utf8' }).stdout.trim();
    env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ??= path.join(
      sysroot,
      'lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/x86_64-w64-mingw32-gcc.exe'
    );
  }
}
const result = spawnSync(
  'cargo',
  ['build', '--locked', '--release', '--manifest-path', 'native/Cargo.toml'],
  { env, stdio: 'inherit', cwd: path.resolve(__dirname, '..') }
);
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
