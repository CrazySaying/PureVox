# PureVox — AI 麦克风降噪工具
# Copyright (C) 2024-2026 a2heng <752848283@qq.com>
#
# PureVox is licensed under the GNU General Public License v3.0 or
# later (GPL-3.0-or-later).  See LICENSE for details.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# The built-in AI models are NOT covered by the GPL; they are the
# property of a2heng and may only be used with PureVox under
# authorization.  See MODEL-LICENSE.md for details.
#
# SPDX-License-Identifier: GPL-3.0-or-later

<#
.SYNOPSIS
    生成 PureVox 的**全部跨平台图标**：应用图标（通用 / Windows / macOS / iOS）、
    系统托盘图标、Android 启动图标，以及它们共同的源图。Linux 用 gen_icons.sh（同一套定义）。

.DESCRIPTION
    ## P 是「四个盒子」，不是字体

    P 的字形最早用 legacy 的 ark-pixel 像素字体渲染（`tools/gen_lite_tray_icon.py`，托盘黄 P），
    但**不再走字体**了：

    - 字体只存在于**只读快照**里（AGENTS §2.3 / §2.5），现役树依赖它不合适；
    - PIL / GDI+ / FreeType 三套渲染的**字形度量各不相同**（实测同一参数在 PIL 与 GDI+ 下
      差 8%），同一份代码在不同平台会画出**不同的 P**——对「跨平台同款图标」是硬伤；
    - P 本身是**直角几何图形**，本就该用几何描述，而不是先渲染成字形再缩放。

    现在的定义就是从 `audio_icon_base.png` 上逐像素量出来的四个盒子（512 画布坐标），
    见下方 `$PBoxes`。纯矢量：改一个数字就是改设计，`.ps1` 与 `.sh` 画的是同一张表，
    结果必然一致。

    ## 与原版的两处差异（都是修正）

    1. **P 改为居中**。原图的 P 垂直偏下约 15px（生成时按 bbox 居中，但算的是字形外框而非
       成品外框），看着重心偏下。
    2. **描边不再被裁**。原图画布是 512×512，而 P 含描边的自然包围盒高 495、底边在 y=518——
       描边末端被画布切掉 7px，底部是平的不是圆角收口。这里按自然尺寸居中，不裁。

    ## 圆边为什么要超采样

    直接按目标尺寸画圆，圆周就是一圈台阶（mdpi 48px 上尤其明显）。一律 **4× 超采样再降采样**：
    圆边变成多像素的灰阶过渡，观感是平滑弧线而不是锯齿。

    ## 各平台图标类型

    | 平台 | 文件 |
    | --- | --- |
    | 通用 / 打包 | `32x32` `64x64` `128x128` `128x128@2x` `icon.png`(512) `icon.ico` `icon.icns` |
    | Windows Store | `Square{30,44,71,89,107,142,150,284,310}x…Logo.png` `StoreLogo.png` |
    | iOS | `AppIcon-{20,29,40,60,76,83.5}x…` `AppIcon-512@2x`（满幅无圆角） |
    | 系统托盘 | `tray_running.png`（蓝渐变） `tray_stopped.png`（暗灰 = 已停止） |
    | Android | `mipmap-*/ic_launcher{,_round,_foreground}.png` + `anydpi-v26` + 背景矢量 |

    托盘不是蓝色 P 的缩小版：**底色换成暗灰**表示「已停止」，运行时切回蓝渐变。渐变在 64px
    上看不出层次，两个状态靠**底色**一眼可辨才是要点。

.PARAMETER Compare
    只把生成的 P 与现有 audio_icon_base.png 做形状比对，不写任何文件。

.PARAMETER Skip
    跳过某些部分：Source / Bundle / Store / Ios / Macos / Tray / Android（可多选）

.EXAMPLE
    pwsh tools/automation/gen_icons.ps1
    pwsh tools/automation/gen_icons.ps1 -Compare
    pwsh tools/automation/gen_icons.ps1 -Skip Android,Ios
#>

[CmdletBinding()]
param(
    [switch]$Compare,
    [string[]]$Skip = @()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Drawing

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$icons = Join-Path $repo 'src-tauri\icons'
$res = Join-Path $repo 'android\app\src\main\res'

# --------------------------------------------------------------------------
# P 的矢量定义 —— 512 画布坐标，逐像素量自 audio_icon_base.png
#
# 本体是四个直角盒子；描边 = 每个盒子向外扩 $StrokeW（方形外扩，不是圆角化）。
# 右上角那个 1px 错位（上横止于 336、右竖起于 338）是像素风刻意的，别「顺手对齐」。
# --------------------------------------------------------------------------
$PBoxes = @(
    @(120.0,  54.0, 336.0, 107.0),   # 上横  217×54
    @(120.0,  54.0, 173.0, 488.0),   # 左竖   54×435
    @(338.0, 109.0, 391.0, 270.0),   # 右竖   54×162
    @(120.0, 272.0, 336.0, 325.0)    # 中横  217×54
)
$StrokeW   = 30.0
$PBoxAll   = @(90.0, 24.0, 421.0, 518.0)   # 含描边的整体包围盒（332×495）

$FILL = [System.Drawing.Color]::FromArgb(255, 0x64, 0xB4, 0xFF)   # P 本体 浅蓝
$EDGE = [System.Drawing.Color]::FromArgb(255, 0x0F, 0x23, 0x50)   # P 描边 深蓝

$BG_TOP    = [System.Drawing.Color]::FromArgb(255, 0x10, 0x2F, 0x5D)
$BG_BOTTOM = [System.Drawing.Color]::FromArgb(255, 0x1E, 0x6F, 0xD0)
$CORNER_R  = 110.0 / 512.0      # 圆角半径 / 边长

# 托盘「已停止」态：底色换暗灰，**P 也换成暗蓝**——只压底色的话 P 仍是亮蓝，在暗灰底上
# 比运行时还跳，一眼看不出停了。整套一起压暗才有「熄灭」的观感。
$TRAY_STOPPED_TOP    = [System.Drawing.Color]::FromArgb(255, 0x14, 0x14, 0x17)
$TRAY_STOPPED_BOTTOM = [System.Drawing.Color]::FromArgb(255, 0x24, 0x24, 0x29)
$TRAY_STOPPED_FILL   = [System.Drawing.Color]::FromArgb(255, 0x2F, 0x5C, 0x8F)   # P 本体 暗蓝
$TRAY_STOPPED_EDGE   = [System.Drawing.Color]::FromArgb(255, 0x12, 0x16, 0x1F)   # P 描边 近黑

$P_IN_LEGACY   = 0.797    # 传统图标里 P 高 / 边长（沿用现有观感）
$P_IN_ADAPTIVE = 0.57     # 自适应前景里 P 高 / 108dp 画布（留足安全区）
$P_IN_TRAY     = 0.72     # 托盘：64px 下 P 要占七成才看得清，早先 0.62 明显偏小
$SUPERSAMPLE   = 4

# --------------------------------------------------------------------------
# 绘制
# --------------------------------------------------------------------------

function Add-Player($Gfx, [double]$Scale, [double]$OffX, [double]$OffY, $Fill = $FILL, $Edge = $EDGE) {
    $be = [System.Drawing.SolidBrush]::new($Edge)
    $bf = [System.Drawing.SolidBrush]::new($Fill)
    # 先画描边：每个盒子向外扩 $StrokeW（方形），相邻盒子的外扩自然合并
    foreach ($r in $PBoxes) {
        $x = $OffX + ($r[0] - $StrokeW) * $Scale
        $y = $OffY + ($r[1] - $StrokeW) * $Scale
        $w = ($r[2] - $r[0] + 1 + 2 * $StrokeW) * $Scale
        $h = ($r[3] - $r[1] + 1 + 2 * $StrokeW) * $Scale
        $Gfx.FillRectangle($be, $x, $y, $w, $h)
    }
    # 再压本体：覆盖内部，剩下的就是描边
    foreach ($r in $PBoxes) {
        $x = $OffX + $r[0] * $Scale
        $y = $OffY + $r[1] * $Scale
        $w = ($r[2] - $r[0] + 1) * $Scale
        $h = ($r[3] - $r[1] + 1) * $Scale
        $Gfx.FillRectangle($bf, $x, $y, $w, $h)
    }
    $be.Dispose(); $bf.Dispose()
}

function New-RoundRectPath($X, $Y, $W, $H, $R) {
    $p = [System.Drawing.Drawing2D.GraphicsPath]::new()
    $d = $R * 2
    $p.AddArc($X, $Y, $d, $d, 180, 90)
    $p.AddArc($X + $W - $d, $Y, $d, $d, 270, 90)
    $p.AddArc($X + $W - $d, $Y + $H - $d, $d, $d, 0, 90)
    $p.AddArc($X, $Y + $H - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    $p
}

<#
    [Shape] = Full（满幅无圆角，iOS 用）/ Squircle（圆角矩形）/ Round（圆）
    [PRatio] = P 高 / 边长。[Dark] = 托盘停止态配色。
#>
function New-IconBitmap {
    param([int]$Size, [string]$Shape, [double]$PRatio, [bool]$Dark = $false)

    $big = [int]($Size * $SUPERSAMPLE)
    $bmp = New-Object System.Drawing.Bitmap($big, $big,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality

    $top = if ($Dark) { $TRAY_STOPPED_TOP } else { $BG_TOP }
    $bot = if ($Dark) { $TRAY_STOPPED_BOTTOM } else { $BG_BOTTOM }
    # 用 Rectangle 重载：PointF 那组在 PowerShell 里会歧义解析到 rect 分支去
    $br = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
        [System.Drawing.Rectangle]::new(0, 0, $big, $big), $top, $bot,
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    switch ($Shape) {
        'None'  { }   # 自适应前景层：只有 P，背景交给 ic_launcher_bg
        'Round' { $e = [System.Drawing.Drawing2D.GraphicsPath]::new()
                  $e.AddEllipse(0, 0, $big, $big); $g.FillPath($br, $e); $e.Dispose() }
        'Full'  { $g.FillRectangle($br, 0, 0, $big, $big) }
        default { $p = New-RoundRectPath 0 0 $big $big ($CORNER_R * $big)
                  $g.FillPath($br, $p); $p.Dispose() }
    }
    $br.Dispose()

    # P 按自然包围盒等比缩放后居中
    $pW = $PBoxAll[2] - $PBoxAll[0] + 1
    $pH = $PBoxAll[3] - $PBoxAll[1] + 1
    $scale = ($big * $PRatio) / $pH
    $offX = ($big - $pW * $scale) / 2 - $PBoxAll[0] * $scale
    $offY = ($big - $pH * $scale) / 2 - $PBoxAll[1] * $scale
    if ($Dark) { Add-Player -Gfx $g -Scale $scale -OffX $offX -OffY $offY -Fill $TRAY_STOPPED_FILL -Edge $TRAY_STOPPED_EDGE }
    else { Add-Player -Gfx $g -Scale $scale -OffX $offX -OffY $offY }
    $g.Dispose()

    if ($Size -eq $big) { return $bmp }
    $out = New-Object System.Drawing.Bitmap($Size, $Size,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g2 = [System.Drawing.Graphics]::FromImage($out)
    $g2.Clear([System.Drawing.Color]::Transparent)
    $g2.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g2.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g2.DrawImage($bmp, 0, 0, $Size, $Size)
    $g2.Dispose(); $bmp.Dispose()
    $out
}

function Save-Png($Bmp, [string]$Path) {
    New-Item -ItemType Directory -Force -Path (Split-Path $Path -Parent) | Out-Null
    $Bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $Bmp.Dispose()
}

# --------------------------------------------------------------------------
# 容器打包：.ico / .icns
# --------------------------------------------------------------------------

<#
    Windows .ico：ICONDIR + ICONDIRENTRY[] + 各尺寸 PNG。
    条目里的宽高是**单字节**，256 编码成 0。
#>
function Write-Ico([string]$Path, [int[]]$Sizes, [scriptblock]$Make) {
    $imgs = @()
    foreach ($s in $Sizes) { $imgs += , (& $Make $s) }
    $count = $Sizes.Count
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)
    $bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$count)
    $offset = 6 + 16 * $count
    for ($i = 0; $i -lt $count; $i++) {
        $w = if ($Sizes[$i] -ge 256) { 0 } else { $Sizes[$i] }
        $bw.Write([byte]$w); $bw.Write([byte]$w); $bw.Write([byte]0); $bw.Write([byte]0)
        $bw.Write([uint16]1); $bw.Write([uint16]32)
        $bw.Write([uint32]$imgs[$i].Length); $bw.Write([uint32]$offset)
        $offset += $imgs[$i].Length
    }
    foreach ($b in $imgs) { $bw.Write($b) }
    $bw.Flush()
    [IO.File]::WriteAllBytes($Path, $ms.ToArray())
    $ms.Dispose()
}

<#
    macOS .icns：'icns' + 总长(大端) + 若干「4 字节类型 + 4 字节块长(含头) + PNG」。
    长度是**大端** 32 位，写成小端的话 Finder 直接不认这个文件。
#>
function Write-Icns([string]$Path, $TypeMap, [scriptblock]$Make) {
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)
    $bw.Write([System.Text.Encoding]::ASCII.GetBytes('icns'))
    $bw.Write([uint32]0)
    $blocks = @()
    foreach ($k in $TypeMap.Keys) { $blocks += , @($k, (& $Make $TypeMap[$k])) }
    $total = 8
    foreach ($b in $blocks) { $total += 8 + $b[1].Length }
    $bw.Write([uint32]$total)
    foreach ($b in $blocks) {
        $bw.Write([System.Text.Encoding]::ASCII.GetBytes([string]$b[0]))
        $bw.Write([uint32](8 + $b[1].Length))
        $bw.Write($b[1])
    }
    $bw.Flush()
    [IO.File]::WriteAllBytes($Path, $ms.ToArray())
    $ms.Dispose()
}

function Get-PngBytes($Bmp) {
    $ms = New-Object System.IO.MemoryStream
    $Bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $r = $ms.ToArray(); $ms.Dispose(); $Bmp.Dispose(); $r
}

# --------------------------------------------------------------------------
# 执行
# --------------------------------------------------------------------------

function New-PlayerBitmapOnly {
    # 源图：只有 P，按自然尺寸摆在 512 画布正中
    $bmp = New-Object System.Drawing.Bitmap(512, 512,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::Transparent)
    $pW = $PBoxAll[2] - $PBoxAll[0] + 1
    $pH = $PBoxAll[3] - $PBoxAll[1] + 1
    Add-Player -Gfx $g -Scale 1.0 -OffX ((512 - $pW) / 2 - $PBoxAll[0]) -OffY ((512 - $pH) / 2 - $PBoxAll[1])
    $g.Dispose()
    $bmp
}

Write-Host "P 矢量定义：$($PBoxes.Count) 个盒子，描边 $($StrokeW)px，包围盒 $($PBoxAll[2]-$PBoxAll[0]+1)x$($PBoxAll[3]-$PBoxAll[1]+1)"

if ($Compare) {
    $mine = New-PlayerBitmapOnly
    $ref = New-Object System.Drawing.Bitmap((Join-Path $icons 'audio_icon_base.png'))
    $both = 0; $onlyMine = 0; $onlyRef = 0
    for ($y = 0; $y -lt 512; $y += 2) {
        for ($x = 0; $x -lt 512; $x += 2) {
            $a = $mine.GetPixel($x, $y).A -gt 128
            $b = $ref.GetPixel($x, $y).A -gt 128
            if ($a -and $b) { $both++ } elseif ($a) { $onlyMine++ } elseif ($b) { $onlyRef++ }
        }
    }
    Write-Host ("形状 IoU = {0:P2}（仅新生成 {1} / 仅原版 {2}）" -f `
        ($both / [double]($both + $onlyMine + $onlyRef)), $onlyMine, $onlyRef)
    Write-Host "注：原版底部描边被画布裁掉 7px、整体偏下，本脚本已修正，故 IoU 不会是 100%。"
    $mine.Dispose(); $ref.Dispose()
    return
}

$mkApp = { param($s) New-IconBitmap -Size $s -Shape 'Squircle' -PRatio $P_IN_LEGACY }

if ($Skip -notcontains 'Source') {
    Save-Png (New-PlayerBitmapOnly) (Join-Path $icons 'audio_icon_base.png')
    Write-Host "[源图  ] audio_icon_base.png"
}

if ($Skip -notcontains 'Bundle') {
    foreach ($s in 32, 64, 128, 256, 512) { Save-Png (& $mkApp $s) (Join-Path $icons "${s}x${s}.png") }
    Copy-Item (Join-Path $icons '256x256.png') (Join-Path $icons '128x128@2x.png') -Force
    Copy-Item (Join-Path $icons '512x512.png') (Join-Path $icons 'icon.png') -Force
    $icoSizes = 16, 24, 32, 48, 64, 128, 256
    Write-Ico (Join-Path $icons 'icon.ico') $icoSizes { param($s) Get-PngBytes (& $mkApp $s) }
    Write-Host "[通用  ] 32/64/128/256/512 + 128x128@2x + icon.png + icon.ico"
}

if ($Skip -notcontains 'Store') {
    foreach ($s in 30, 44, 71, 89, 107, 142, 150, 284, 310, 50) {
        $name = if ($s -eq 50) { 'StoreLogo.png' } else { "Square${s}x${s}Logo.png" }
        Save-Png (& $mkApp $s) (Join-Path $icons $name)
    }
    Write-Host "[Store  ] Square30..310 + StoreLogo"
}

if ($Skip -notcontains 'Macos') {
    $icns = [ordered]@{
        'icp4' = 16; 'icp5' = 32; 'icp6' = 64; 'ic07' = 128
        'ic08' = 256; 'ic09' = 512; 'ic10' = 1024
    }
    Write-Icns (Join-Path $icons 'icon.icns') $icns {
        param($s) Get-PngBytes (New-IconBitmap -Size $s -Shape 'Full' -PRatio $P_IN_LEGACY)
    }
    Write-Host "[macOS  ] icon.icns（icp4..ic10）"
}

if ($Skip -notcontains 'Ios') {
    # Xcode 的 AppIcon 集：**必须是 appiconset 目录 + Contents.json**，裸 PNG Xcode 根本不认。
    # 同一物理尺寸在不同 idiom 下是**同一个文件**（20x20@2x 的 iPhone 与 iPad 都是 40px），
    # 只有 iPad 独有的槽位才另起 `~ipad` 文件——这就是旧图标里那几个 `@2x-1.png` 的由来：
    # 那是 `cargo tauri icon` 自己的 iPad 命名，既不是 Xcode 约定，内容又和 `@2x` 一模一样。
    # 这里按 Xcode 标准来，`-1` 不会再出现。
    $iosDir = Join-Path $icons 'ios\AppIcon.appiconset'
    if (Test-Path $iosDir) { Remove-Item $iosDir -Recurse -Force }   # 先清干净，避免残留旧文件
    New-Item -ItemType Directory -Force -Path $iosDir | Out-Null

    # 槽位表用「文件名|边长|idiom|scale|槽位」的管道分隔字符串，不用嵌套数组——
    # PowerShell 在 `@()` 里放嵌套数组容易把内层展平，`$s[1]` 会拿到 Int32 而不是数组。
    $slots = @(
        'AppIcon-20x20@1x.png|20|universal|1x|20x20'
        'AppIcon-20x20@2x.png|40|universal|2x|20x20'
        'AppIcon-20x20@3x.png|60|iphone|3x|20x20'
        'AppIcon-20x20@1x~ipad.png|20|ipad|1x|20x20'
        'AppIcon-20x20@2x~ipad.png|40|ipad|2x|20x20'
        'AppIcon-29x29@1x.png|29|universal|1x|29x29'
        'AppIcon-29x29@2x.png|58|universal|2x|29x29'
        'AppIcon-29x29@3x.png|87|iphone|3x|29x29'
        'AppIcon-29x29@1x~ipad.png|29|ipad|1x|29x29'
        'AppIcon-29x29@2x~ipad.png|58|ipad|2x|29x29'
        'AppIcon-40x40@2x.png|80|universal|2x|40x40'
        'AppIcon-40x40@3x.png|120|iphone|3x|40x40'
        'AppIcon-40x40@1x~ipad.png|40|ipad|1x|40x40'
        'AppIcon-40x40@2x~ipad.png|80|ipad|2x|40x40'
        'AppIcon-60x60@2x.png|120|iphone|2x|60x60'
        'AppIcon-60x60@3x.png|180|iphone|3x|60x60'
        'AppIcon-76x76@1x~ipad.png|76|ipad|1x|76x76'
        'AppIcon-76x76@2x~ipad.png|152|ipad|2x|76x76'
        'AppIcon-83.5x83.5@2x~ipad.png|167|ipad|2x|83.5x83.5'
        'AppIcon-1024x1024.png|1024|ios-marketing|1x|1024x1024'
    )
    $entries = @()
    foreach ($row in $slots) {
        $fn, $sz, $idiom, $scale, $slot = $row -split '\|'
        Save-Png (New-IconBitmap -Size ([int]$sz) -Shape 'Full' -PRatio $P_IN_LEGACY) (Join-Path $iosDir $fn)
        $entries += "    { `"filename`": `"$fn`", `"idiom`": `"$idiom`", `"scale`": `"$scale`", `"size`": `"$slot`" }"
    }
    $json = "{
  `"images`": [
" + ($entries -join ",`n") + "
  ],
  `"info`": { `"author`": `"purevox`", `"version`": 1 }
}
"
    [IO.File]::WriteAllText((Join-Path $iosDir 'Contents.json'), $json,
        (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "[iOS    ] AppIcon.appiconset（$($slots.Count) 个槽位 + Contents.json）"
}

if ($Skip -notcontains 'Tray') {
    Save-Png (New-IconBitmap -Size 64 -Shape 'Squircle' -PRatio $P_IN_TRAY -Dark $false) (Join-Path $icons 'tray_running.png')
    Save-Png (New-IconBitmap -Size 64 -Shape 'Squircle' -PRatio $P_IN_TRAY -Dark $true)  (Join-Path $icons 'tray_stopped.png')
    Write-Host "[托盘   ] tray_running / tray_stopped（64px，蓝 / 暗灰）"
}

if ($Skip -notcontains 'Android') {
    $densities = [ordered]@{
        'mdpi' = @(48, 108); 'hdpi' = @(72, 162); 'xhdpi' = @(96, 216)
        'xxhdpi' = @(144, 324); 'xxxhdpi' = @(192, 432)
    }
    foreach ($d in $densities.Keys) {
        $legacy = $densities[$d][0]; $fg = $densities[$d][1]
        $dir = Join-Path $res "mipmap-$d"
        Save-Png (New-IconBitmap -Size $legacy -Shape 'Squircle' -PRatio $P_IN_LEGACY) (Join-Path $dir 'ic_launcher.png')
        Save-Png (New-IconBitmap -Size $legacy -Shape 'Round'     -PRatio $P_IN_LEGACY) (Join-Path $dir 'ic_launcher_round.png')
        Save-Png (New-IconBitmap -Size $fg     -Shape 'None'      -PRatio $P_IN_ADAPTIVE) (Join-Path $dir 'ic_launcher_foreground.png')
    }
    Write-Host "[Android] mipmap 五档 × 3（round 用 4× 超采样，圆边不再是台阶）"
}

Write-Host "`n完成。"