# Run cargo for the workspace with the Windows GNU toolchain fix-ups.
# Usage: powershell -NoProfile -File scripts/cargo.ps1 <cargo args...>
#
# On x86_64-pc-windows-gnu hosts the bundled SQLite needs a real C compiler.
# Rust's self-contained gcc is only a linker, so an llvm-mingw installation in
# %LOCALAPPDATA%\SuperAgent\toolchain (or SUPERAGENT_LLVM_MINGW) supplies
# clang and llvm-ar, and its ld.lld links while Rust keeps its own CRT.
# Do not switch back to Rust's GNU ld: it mislinks the raw-dylib import
# libraries and the executable crashes before main. MSVC hosts need nothing.
$ErrorActionPreference = 'Stop'
$cargoBin = Join-Path ($(if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' })) 'bin'
$env:PATH = "$env:PATH;$cargoBin"
$host_ = (& rustc -vV 2>$null) -join "`n"
if ($LASTEXITCODE -ne 0) { throw '请安装 Rust stable（含 cargo）后重试' }
if ($host_ -match 'host: x86_64-pc-windows-gnu') {
    $toolRoot = Join-Path $(if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { $HOME }) 'SuperAgent\toolchain'
    $candidates = @()
    if ($env:SUPERAGENT_LLVM_MINGW) {
        $candidates = @($env:SUPERAGENT_LLVM_MINGW)
    } elseif (Test-Path -LiteralPath $toolRoot) {
        $candidates = Get-ChildItem -LiteralPath $toolRoot -Directory -Filter 'llvm-mingw-*' |
            Sort-Object Name -Descending | ForEach-Object { $_.FullName }
    }
    $root = $candidates | Where-Object { Test-Path -LiteralPath (Join-Path $_ 'bin\clang.exe') } | Select-Object -First 1
    if (-not $root) { throw "缺少 C 编译器：请安装 llvm-mingw 到 $toolRoot 或设置 SUPERAGENT_LLVM_MINGW" }
    $bin = Join-Path $root 'bin'
    $env:PATH = "$bin;$env:PATH"
    if (-not $env:CC_x86_64_pc_windows_gnu) { $env:CC_x86_64_pc_windows_gnu = Join-Path $bin 'x86_64-w64-mingw32-clang.exe' }
    if (-not $env:AR_x86_64_pc_windows_gnu) { $env:AR_x86_64_pc_windows_gnu = Join-Path $bin 'llvm-ar.exe' }
    if (-not $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER) {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $bin 'ld.lld.exe'
    }
    $flags = '-C linker-flavor=ld.lld -C link-self-contained=yes'
    $prior = $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = $(if ($prior) { "$prior $flags" } else { $flags })
}
Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    & cargo @args
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
