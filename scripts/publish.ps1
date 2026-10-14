# Builds the release: dist/Nuiter-<version>-win-<arch>.zip (the name and layout Updates installs), holding one folder:
#   Nuiter\Nuiter.exe   the app (also its own reader helper, "Nuiter.exe extract-service"); no runtime to carry
#   Nuiter\llama\       llama-server (x64: scripts/build-llama.ps1, pinned tag + the pooled-embeddings patch)
#   Nuiter\README.md, LICENSE
# EmbeddingGemma 2 (~865 MB) is downloaded by the app on first start. ARM64 downloads ggml-org's llama.cpp build too.
# nuiter-cli is a developer tool and isn't in the release.
param([string]$Arch = "x64")
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$target = if ($Arch -eq "arm64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$out = Join-Path $root "target\publish-$Arch"
$dist = Join-Path $root "dist"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $dist | Out-Null
if ($Arch -eq "x64") {
    & (Join-Path $root "scripts\build-llama.ps1")   # does nothing when that build is already there
    if (-not (Test-Path (Join-Path $root "vendor\llama-bin\llama-server.exe"))) { throw "llama.cpp build missing" }
}

Push-Location $root
try {
    cargo build --release -p nuiter --target $target
    if ($LASTEXITCODE) { throw "cargo build failed" }
} finally { Pop-Location }

$built = Join-Path $root "target\$target\release"
$app = Join-Path $out "Nuiter"
New-Item -ItemType Directory -Force $app | Out-Null
Copy-Item (Join-Path $built "Nuiter.exe") $app
if ($Arch -eq "x64") { Copy-Item -Recurse (Join-Path $root "vendor\llama-bin") (Join-Path $app "llama") }
Copy-Item (Join-Path $root "LICENSE"), (Join-Path $root "README.md") $app

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$zip = Join-Path $dist "Nuiter-$version-win-$Arch.zip"
if (Test-Path $zip) { Remove-Item $zip }
# Entries named "Nuiter/..." with forward slashes: Windows PowerShell's Compress-Archive and ZipFile.CreateFromDirectory
# write "\" into entry names, which other unzip tools turn into odd file names.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::Open($zip, [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($file in Get-ChildItem $app -Recurse -File) {
        $name = "Nuiter/" + $file.FullName.Substring($app.Length + 1).Replace("\", "/")
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $file.FullName, $name, [IO.Compression.CompressionLevel]::Optimal)
    }
} finally { $archive.Dispose() }
if (-not (Test-Path (Join-Path $app "llama\llama-server.exe")) -and $Arch -eq "x64") { throw "llama-server.exe is missing beside Nuiter.exe" }
Write-Host "Created $zip ($([math]::Round((Get-Item $zip).Length / 1MB)) MB)"
