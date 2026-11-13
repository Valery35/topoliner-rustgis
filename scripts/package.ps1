# Topoliner для RustGIS - расширение-процесс RustGIS.
# Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Пакет .rgplugin расширения: сборка и упаковка одной командой.
#
#   powershell -File scripts\package.ps1
#
# В -Dist появляются:
#   topoliner/                 plugin.json и программа - папка для
#                              «Add Plugin Folder…» и rustgis-cli
#   topoliner-<версия>.rgplugin пакет для установки из файла
#
# Упаковку делает сценарий крейта rustgis-kit: он один на всю линейку.
# Путь к крейту берётся из -Kit, иначе из соседней папки репозитория.

param(
    [string]$Dist = 'dist',
    [string]$Kit = '',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not $SkipBuild) {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }
    }
    $program = Join-Path $root 'target\release\topoliner-rustgis.exe'
    if (-not (Test-Path $program)) { throw "no program at ${program} - cargo build --release" }

    if (-not $Kit) { $Kit = Join-Path (Split-Path -Parent $root) 'rustgis-kit' }
    $packer = Join-Path $Kit 'scripts\package.ps1'
    if (-not (Test-Path $packer)) { throw "no rustgis-kit at ${Kit} - pass -Kit" }

    & $packer -Manifest (Join-Path $root 'plugin.json') -Program $program -Dist (Join-Path $root $Dist)
}
finally {
    Pop-Location
}
