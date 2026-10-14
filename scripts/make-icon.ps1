# Regenerates the app icons (multi-size .ico, PNG-compressed entries) and the README images.
# The mark is a one-colour owl "block": the top edge is notched into ear tufts, the eyes are see-through holes.
# nuiter.ico is the dark mark (light themes, also the exe icon), nuiter-light.ico the light one for dark themes.
# Small sizes snap the edges to whole pixels so they stay crisp.
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$assets = Join-Path $root "crates\nuiter\assets"
$docs = Join-Path $root "docs"
New-Item -ItemType Directory -Force $assets, $docs | Out-Null

function Render([int]$s, [Drawing.Color]$color) {
    $bmp = New-Object Drawing.Bitmap $s, $s
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = "AntiAlias"; $g.PixelOffsetMode = "Half"
    # Geometry in a 64-unit box; snapped to the pixel grid below 64 px.
    $u = $s / 64.0
    $snap = $s -lt 64
    function P([double]$v) { if ($snap) { [single][Math]::Round($v) } else { [single]$v } }
    $l = P(3 * $u); $t = P(3 * $u); $r = [single]($s - $l); $b = [single]($s - $t)
    $notch = P(18 * $u); $cr = [single](10 * $u)
    $path = New-Object Drawing.Drawing2D.GraphicsPath ([Drawing.Drawing2D.FillMode]::Alternate)
    $path.AddLine($l, $t, [single]($s / 2.0), $notch)
    $path.AddLine([single]($s / 2.0), $notch, $r, $t)
    $path.AddLine($r, $t, $r, $b - $cr)
    $path.AddArc($r - 2 * $cr, $b - 2 * $cr, 2 * $cr, 2 * $cr, 0, 90)
    $path.AddArc($l, $b - 2 * $cr, 2 * $cr, 2 * $cr, 90, 90)
    $path.CloseFigure()
    # Eyes: mirror-symmetric, radius and centre on half pixels when small.
    $er = 9.5 * $u; $ey = 38 * $u; $dx = 12.5 * $u
    if ($snap) { $er = [Math]::Max(1.5, [Math]::Round($er * 2) / 2); $ey = [Math]::Round($ey * 2) / 2; $dx = [Math]::Round($dx * 2) / 2 }
    foreach ($ex in ($s / 2.0 - $dx), ($s / 2.0 + $dx)) {
        $path.AddEllipse([single]($ex - $er), [single]($ey - $er), [single](2 * $er), [single](2 * $er))
    }
    $g.FillPath((New-Object Drawing.SolidBrush $color), $path)
    $g.Dispose()
    $ms = New-Object IO.MemoryStream
    $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    return , $ms.ToArray()
}

function WriteIco([string]$out, [Drawing.Color]$color) {
    $sizes = 16, 20, 24, 32, 40, 48, 64, 256
    $images = $sizes | ForEach-Object { , (Render $_ $color) }
    $fs = [IO.File]::Create($out); $bw = New-Object IO.BinaryWriter $fs
    $bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $s = $sizes[$i]; $len = $images[$i].Length
        $bw.Write([byte]($s % 256)); $bw.Write([byte]($s % 256)); $bw.Write([byte]0); $bw.Write([byte]0)
        $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]$len); $bw.Write([uint32]$offset)
        $offset += $len
    }
    foreach ($img in $images) { $bw.Write($img) }
    $bw.Close()
    Write-Host "Wrote $out"
}

$dark = [Drawing.Color]::FromArgb(255, 26, 26, 26)
$light = [Drawing.Color]::FromArgb(255, 245, 245, 245)
WriteIco (Join-Path $assets "nuiter.ico") $dark
WriteIco (Join-Path $assets "nuiter-light.ico") $light
[IO.File]::WriteAllBytes((Join-Path $docs "icon.png"), (Render 256 $dark))
[IO.File]::WriteAllBytes((Join-Path $docs "icon-light.png"), (Render 256 $light))
