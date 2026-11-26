#!/usr/bin/env pwsh
# Standalone gate for this Rust crate repository.
# In an isolated clone (CI), dependencies resolve as published (git + tag).
# Inside the monorepo, Cargo follows the sdk/rust development workspace.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location -LiteralPath $repoRoot
try {
    $cargo = Get-Command cargo -ErrorAction Stop

    & $cargo.Source fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed with exit code $LASTEXITCODE." }

    & $cargo.Source clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed with exit code $LASTEXITCODE." }

    & $cargo.Source test
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed with exit code $LASTEXITCODE." }
}
finally {
    Pop-Location
}
