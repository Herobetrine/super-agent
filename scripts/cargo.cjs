'use strict';
// Run cargo for the Rust workspace with the same Windows GNU toolchain
// discovery as build-native.cjs. Usage: node scripts/cargo.cjs <cargo args...>
// On windows-gnu hosts, bundled C dependencies (SQLite) need a real C compiler.
// Rust's self-contained gcc is only a linker, so an llvm-mingw installation in
// %LOCALAPPDATA%\SuperAgent\toolchain (or SUPERAGENT_LLVM_MINGW) supplies
// clang, llvm-ar and dlltool while Rust keeps its own linker and runtime.
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const { spawnSync } = require('node:child_process');

function toolchainEnv(base = process.env) {
  const env = { ...base };
  const cargoBin = path.join(env.CARGO_HOME || path.join(os.homedir(), '.cargo'), 'bin');
  env.PATH = [env.PATH, cargoBin].join(path.delimiter);
  const rust = spawnSync('rustc', ['-vV'], { env, encoding: 'utf8' });
  if (rust.status !== 0) throw new Error('请安装 Rust stable（含 cargo）后重试');
  if (process.platform !== 'win32' || !rust.stdout.includes('host: x86_64-pc-windows-gnu')) return env;
  const toolRoot = path.join(env.LOCALAPPDATA || os.homedir(), 'SuperAgent', 'toolchain');
  const candidates = env.SUPERAGENT_LLVM_MINGW
    ? [env.SUPERAGENT_LLVM_MINGW]
    : fs.existsSync(toolRoot)
      ? fs
          .readdirSync(toolRoot)
          .filter((v) => v.startsWith('llvm-mingw-'))
          .sort()
          .reverse()
          .map((dir) => path.join(toolRoot, dir))
      : [];
  const root = candidates.find((p) => fs.existsSync(path.join(p, 'bin', 'clang.exe')));
  if (!root) throw new Error('缺少 C 编译器：请安装 llvm-mingw 到 ' + toolRoot + ' 或设置 SUPERAGENT_LLVM_MINGW');
  const bin = path.join(root, 'bin');
  env.PATH = bin + path.delimiter + env.PATH;
  env.CC_x86_64_pc_windows_gnu ??= path.join(bin, 'x86_64-w64-mingw32-clang.exe');
  env.AR_x86_64_pc_windows_gnu ??= path.join(bin, 'llvm-ar.exe');
  // Import libraries for raw-dylib (windows-link) come from llvm-mingw's
  // dlltool. GNU ld from Rust's self-contained gcc mislinks them: the
  // executable starts with unpatched import slots and crashes before main.
  // Link with llvm-mingw's lld while keeping Rust's self-contained CRT.
  env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER ??= path.join(bin, 'ld.lld.exe');
  const flags = '-C linker-flavor=ld.lld -C link-self-contained=yes';
  const prior = env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS;
  env.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = prior ? prior + ' ' + flags : flags;
  return env;
}

if (require.main === module) {
  const result = spawnSync('cargo', process.argv.slice(2), {
    env: toolchainEnv(),
    stdio: 'inherit',
    cwd: path.resolve(__dirname, '..')
  });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
}

module.exports = { toolchainEnv };
