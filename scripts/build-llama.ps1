# Builds llama-server (llama.cpp, MIT) for Nuiter: the pinned tag plus scripts/llama.cpp-pooled-embeddings.patch.
# Without the patch, every input token reserves and clears a row of logits for Gemma's 262k vocabulary (1 MiB) that an
# embedding model never produces: one 2,000-token chunk took llama-server from 0.6 to 2.4 GB.
#
# Output: vendor/llama-bin (llama-server.exe and its DLLs, ggml's backends as modules: every CPU variant + Vulkan), out
# of git. The app and nuiter-cli copy it next to themselves as llama\ (build-support/copy_llama.rs), and the installer
# uses it instead of downloading ggml-org's build. Does nothing when that build is already there.
#
# Needs Visual Studio (C++ workload, its CMake and Ninja), git and the Vulkan SDK (winget install KhronosGroup.VulkanSDK).
# x64 only: ARM64 keeps downloading ggml-org's build.
param([switch]$Force)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

# Keep in step with installer::LLAMA_BUILD. A new tag ships only if the reference vector still matches
# (Indexer::verify_model re-embeds every index otherwise).
$Tag = "b11577"
$Commit = "cbe28bb3cb0758f0c566a9a3561766821aeb0748"   # the tag's commit: a tag can be moved, a commit can't
$Patch = Join-Path $PSScriptRoot "llama.cpp-pooled-embeddings.patch"
$Source = Join-Path $root "vendor\llama.cpp"
$Build = Join-Path $root "vendor\llama-build"
$Out = Join-Path $root "vendor\llama-bin"

$stamp = "$Tag $((Get-FileHash $Patch -Algorithm SHA256).Hash.Substring(0, 12).ToLower()) vulkan"
if (-not $Force -and (Test-Path "$Out\llama-server.exe") -and (Get-Content "$Out\BUILD" -ErrorAction SilentlyContinue) -eq $stamp) {
    Write-Host "llama.cpp $Tag (patched) is already built in $Out"
    return
}

if (-not $env:VULKAN_SDK) { $env:VULKAN_SDK = [Environment]::GetEnvironmentVariable("VULKAN_SDK", "Machine") }
if (-not $env:VULKAN_SDK -or -not (Test-Path $env:VULKAN_SDK)) { throw "Vulkan SDK not found: winget install KhronosGroup.VulkanSDK" }

if (-not (Test-Path $Source)) {
    # Line endings as they are (Git for Windows would turn them into CRLF, and the patch wouldn't apply).
    git -c advice.detachedHead=false -c core.autocrlf=false clone --quiet --depth 1 --branch $Tag https://github.com/ggml-org/llama.cpp $Source
} elseif ((git -C $Source describe --tags --exact-match 2>$null) -ne $Tag) {
    git -C $Source fetch --quiet --depth 1 origin tag $Tag
    git -C $Source checkout --quiet --force $Tag   # --force also drops the old patch
}
if ((git -C $Source rev-parse HEAD) -ne $Commit) { throw "llama.cpp's $Tag isn't commit $Commit here: not building it" }
# PowerShell 5.1 turns a native command's stderr into an error under "Stop": checks run through cmd.
function Test-Git([string]$arguments) { cmd /c "git -C `"$Source`" $arguments >nul 2>nul"; $LASTEXITCODE -eq 0 }
if (Test-Git "apply --check `"$Patch`"") { git -C $Source apply $Patch }
elseif (-not (Test-Git "apply --reverse --check `"$Patch`"")) { throw "The patch neither applies to $Tag nor is applied already" }

# MSVC, with the CMake and Ninja that come with Visual Studio.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio with the C++ workload not found" }
$env:PATH = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer;$env:PATH"   # the dev shell calls vswhere itself
Import-Module "$vs\Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments "-arch=x64 -host_arch=x64" | Out-Null

# Shared libraries, the backends as modules: every CPU variant (one build fits any x86-64 machine) and Vulkan.
# Only llama-server: no web UI (Nuiter starts it with --no-webui), no HTTPS (it never downloads anything).
$cmakeArgs = @(
    "-S", $Source, "-B", $Build, "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release",
    "-DBUILD_SHARED_LIBS=ON", "-DGGML_BACKEND_DL=ON", "-DGGML_CPU_ALL_VARIANTS=ON", "-DGGML_NATIVE=OFF", "-DGGML_VULKAN=ON",
    "-DLLAMA_BUILD_SERVER=ON", "-DLLAMA_BUILD_TOOLS=ON", "-DLLAMA_BUILD_TESTS=OFF", "-DLLAMA_BUILD_EXAMPLES=OFF",
    "-DLLAMA_BUILD_APP=OFF", "-DLLAMA_BUILD_UI=OFF", "-DLLAMA_USE_PREBUILT_UI=OFF", "-DLLAMA_OPENSSL=OFF",
    "-DLLAMA_BUILD_BORINGSSL=OFF", "-DLLAMA_CURL=OFF"
)
cmake @cmakeArgs
if ($LASTEXITCODE) { throw "cmake configure failed" }
$cpu = "x64", "sse42", "sandybridge", "haswell", "skylakex", "cannonlake", "cascadelake", "icelake", "alderlake" | ForEach-Object { "ggml-cpu-$_" }
cmake --build $Build --config Release --target llama-server ggml-vulkan @cpu
if ($LASTEXITCODE) { throw "cmake build failed" }

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory $Out | Out-Null
Copy-Item "$Build\bin\llama-server.exe", "$Build\bin\*.dll" $Out
# The C++ runtime beside them (app-local, as Microsoft allows): not every PC has the VC++ Redistributable. The DLLs
# pass C++ objects to each other, so a static runtime in each isn't an option.
$crt = Get-ChildItem "$vs\VC\Redist\MSVC" -Directory | Sort-Object Name -Descending |
    ForEach-Object { Get-ChildItem "$($_.FullName)\x64" -Directory -Filter "Microsoft.VC*.CRT" -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (-not $crt) { throw "VC++ runtime not found under $vs\VC\Redist\MSVC" }
Copy-Item "$($crt.FullName)\msvcp140.dll", "$($crt.FullName)\vcruntime140.dll", "$($crt.FullName)\vcruntime140_1.dll" $Out
Copy-Item (Join-Path $Source "LICENSE") (Join-Path $Out "LICENSE-llama.cpp.txt")
Set-Content "$Out\BUILD" $stamp -NoNewline
Write-Host "Built llama.cpp $Tag (patched, vulkan) into $Out"
