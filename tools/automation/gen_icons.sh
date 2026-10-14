#!/usr/bin/env bash
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
#
# gen_icons.sh —— 生成 PureVox 全部跨平台图标。Windows 用 gen_icons.ps1，
# 两者共用**同一套 P 的矢量定义**（见下方 P_BOXES），结果一致。
#
#   ./tools/automation/gen_icons.sh                 # 全量
#   ./tools/automation/gen_icons.sh --compare       # 只比对，不写文件
#   ./tools/automation/gen_icons.sh --skip Android  # 跳过分组（可重复）
#
# 依赖：ImageMagick（IM7 的 magick 或 IM6 的 convert 都行）、coreutils。
# **不参与 CI**——图标是一次性生成、产物入库的静态资源，改设计时本地跑一次即可。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ICONS="$ROOT/src-tauri/icons"
RES="$ROOT/android/app/src/main/res"

# ---------------------------------------------------------------- P 的矢量定义
# 512 画布坐标，逐像素量自 audio_icon_base.png。改设计就改这里。
# 本体是四个直角盒子；描边 = 每个盒子向外扩 STROKE（方形外扩，不是圆角化）。
# 右上角那个 1px 错位（上横止于 336、右竖起于 338）是像素风刻意的，别「顺手对齐」。
P_BOXES=(
  "120 54 336 107"    # 上横 217x54
  "120 54 173 488"    # 左竖  54x435
  "338 109 391 270"   # 右竖  54x162
  "120 272 336 325"   # 中横 217x54
)
STROKE=30
P_ALL=(90 24 421 518)   # 含描边的整体包围盒 332x495
P_W=332
P_H=495

FILL="#64B4FF"
EDGE="#0F2350"
BG_TOP="#102F5D"
BG_BOTTOM="#1E6FD0"
CORNER_R=110            # 圆角半径 / 512
TRAY_STOPPED_TOP="#141417"
TRAY_STOPPED_BOTTOM="#242429"
# 停止态 P 也换成暗蓝：只压底色的话 P 仍是亮蓝，在暗灰底上比运行时还跳，看不出停了
TRAY_STOPPED_FILL="#2F5C8F"
TRAY_STOPPED_EDGE="#12161F"

P_IN_LEGACY=797   # 千分比：P 高除以边长
P_IN_ADAPTIVE=570
P_IN_TRAY=720     # 托盘 64px 下 P 要占七成才看得清，早先 620 明显偏小
SUP=4                   # 超采样倍数（圆边平滑的关键）

# ---------------------------------------------------------------- 工具探测
if command -v magick >/dev/null 2>&1; then IM=magick
elif command -v convert >/dev/null 2>&1; then IM=convert
else
  echo "需要 ImageMagick：apt install imagemagick" >&2
  exit 1
fi
command -v "$IM" >/dev/null || { echo "ImageMagick 不可用" >&2; exit 1; }

COMPARE=0
SKIP=()
for a in "$@"; do
  case "$a" in
    --compare) COMPARE=1 ;;
    --skip) shift; SKIP+=("$1") ;;
    --skip=*) SKIP+=("${a#--skip=}") ;;
    -h|--help) sed -n '17,30p' "$0"; exit 0 ;;
    *) echo "未知参数：$a" >&2; exit 2 ;;
  esac
done
skipped() { for s in ${SKIP+"${SKIP[@]}"}; do [ "$s" = "$1" ] && return 0; done; return 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 4 字节大端（.icns 的块长与总长都是大端，写成小端 Finder 直接不认）
be32() { printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $((($1 >> 24) & 255)) $((($1 >> 16) & 255)) $((($1 >> 8) & 255)) $(($1 & 255)))"; }
round() { awk -v v="$1" 'BEGIN{ printf "%d", v + 0.5 }'; }

# ---------------------------------------------------------------- P（紧凑）
# 画在 600x600 上再 -trim，得到**无透明边**的 P；后续按占比缩放。
# 不在目标尺寸上直接画：盒子是矢量的，缩放天然一致，不需要「各尺寸独立光栅化」。
draw_player() {
  local out=$1 fillc=$2 edgec=$3
  local edge_args=() fill_args=() x0 y0 x1 y1
  for b in "${P_BOXES[@]}"; do
    read -r x0 y0 x1 y1 <<<"$b"
    edge_args+=(-fill "$edgec" -draw "rectangle $((x0 - STROKE)),$((y0 - STROKE)) $((x1 + STROKE)),$((y1 + STROKE))")
  done
  for b in "${P_BOXES[@]}"; do
    read -r x0 y0 x1 y1 <<<"$b"
    fill_args+=(-fill "$fillc" -draw "rectangle $x0,$y0 $x1,$y1")
  done
  "$IM" -size 600x600 xc:none "${edge_args[@]}" "${fill_args[@]}" -trim +repage "$out"
}

PLAYER="$TMP/player.png"
PLAYER_STOP="$TMP/player_stop.png"
draw_player "$PLAYER" "$FILL" "$EDGE"
draw_player "$PLAYER_STOP" "$TRAY_STOPPED_FILL" "$TRAY_STOPPED_EDGE"

# ---------------------------------------------------------------- 整图
# shape: none | full | round | squircle   pratio: P 高 / 边长
make_icon() {
  local size=$1 shape=$2 pratio=$3 dark=$4 out=$5
  local big=$((size * SUP))
  local top bot
  if [ "$dark" = "1" ]; then top=$TRAY_STOPPED_TOP; bot=$TRAY_STOPPED_BOTTOM
  else top=$BG_TOP; bot=$BG_BOTTOM; fi

  local bg="$TMP/bg_$$.png"
  case "$shape" in
    none)  : ;;   # 自适应前景层：只有 P，背景交给 ic_launcher_bg
    full)  "$IM" -size "${big}x${big}" "gradient:${top}-${bot}" "$bg" ;;
    round) "$IM" -size "${big}x${big}" "gradient:${top}-${bot}" \
             \( -size "${big}x${big}" xc:none -fill white \
                -draw "circle $((big / 2)),$((big / 2)) $((big / 2)),0" \) \
             -alpha off -compose CopyOpacity -composite "$bg" ;;
    *)     r=$((CORNER_R * big / 512))
             "$IM" -size "${big}x${big}" "gradient:${top}-${bot}" \
             \( -size "${big}x${big}" xc:none -fill white \
                -draw "roundrectangle 0,0 $((big - 1)),$((big - 1)) $r,$r" \) \
             -alpha off -compose CopyOpacity -composite "$bg" ;;
  esac

  [ "$shape" = "none" ] || "$IM" "$bg" "$out.tmp0.png"

  # P 高 = 边长 × pratio（pratio 以千分比给出，如 797 = 0.797），全部整数运算
  local ph pw src
  ph=$((big * pratio / 1000))
  pw=$((ph * P_W / P_H))
  if [ "$dark" = "1" ]; then src=$PLAYER_STOP; else src=$PLAYER; fi
  "$IM" "$src" -resize "${pw}x${ph}" "$out.tmp1.png"

  if [ "$shape" = "none" ]; then
    "$IM" "$out.tmp1.png" -background none -gravity center -extent "${big}x${big}" "$out.tmp2.png"
  else
    "$IM" "$out.tmp0.png" "$out.tmp1.png" -gravity center -compose over -composite "$out.tmp2.png"
  fi

  "$IM" "$out.tmp2.png" -filter Lanczos -resize "${size}x${size}" "$out"
  rm -f "$out.tmp0.png" "$out.tmp1.png" "$out.tmp2.png" "$bg"
}

mkdir -p "$ICONS/ios" "$RES/mipmap-anydpi-v26"
echo "P 矢量定义：${#P_BOXES[@]} 个盒子，描边 ${STROKE}px，包围盒 ${P_W}x${P_H}"

if [ "$COMPARE" = 1 ]; then
  "$IM" "$PLAYER" -background "#00000000" -gravity center -extent 512x512 "$TMP/mine.png"
  # IoU：只统计 alpha>128 的像素交集
  a=$("$IM" "$TMP/mine.png" -alpha extract -threshold 50% "$TMP/a.png")
  b=$("$IM" "$ICONS/audio_icon_base.png" -alpha extract -threshold 50% "$TMP/b.png")
  both=$(round "$("$IM" "$TMP/a.png" "$TMP/b.png" -compose Multiply -composite -format "%[fx:mean*w*h]" info:)")
  union=$(round "$("$IM" "$TMP/a.png" "$TMP/b.png" -compose Lighten -composite -format "%[fx:mean*w*h]" info:)")
  awk -v b="$both" -v u="$union" 'BEGIN{printf "形状 IoU = %.2f%%\n", (u>0 ? 100*b/u : 0)}'
  echo "注：原版底部描边被画布裁掉 7px、整体偏下，本脚本已修正，故 IoU 不会是 100%。"
  exit 0
fi

if ! skipped Source; then
  "$IM" "$PLAYER" -background none -gravity center -extent 512x512 "$ICONS/audio_icon_base.png"
  echo "[源图  ] audio_icon_base.png"
fi

mk_app() { make_icon "$1" squircle "$P_IN_LEGACY" 0 "$2"; }

if ! skipped Bundle; then
  for s in 32 64 128 256 512; do mk_app "$s" "$ICONS/${s}x${s}.png"; done
  cp "$ICONS/256x256.png" "$ICONS/128x128@2x.png"
  cp "$ICONS/512x512.png" "$ICONS/icon.png"
  mkdir -p "$TMP/ico"
  ico=()
  for s in 16 24 32 48 64 128 256; do
    mk_app "$s" "$TMP/ico/$s.png"; ico+=("$TMP/ico/$s.png")
  done
  "$IM" "${ico[@]}" "$ICONS/icon.ico"
  echo "[通用  ] 32/64/128/256/512 + 128x128@2x + icon.png + icon.ico"
fi

if ! skipped Store; then
  for s in 30 44 71 89 107 142 150 284 310; do mk_app "$s" "$ICONS/Square${s}x${s}Logo.png"; done
  mk_app 50 "$ICONS/StoreLogo.png"
  echo "[Store  ] Square30..310 + StoreLogo"
fi

if ! skipped Macos; then
  # ICNS 容器自己拼：ImageMagick 写出多尺寸的 icns 不可靠
  mkdir -p "$TMP/icns"
  specs=(icp4:16 icp5:32 icp6:64 ic07:128 ic08:256 ic09:512 ic10:1024)
  total=8
  for sp in "${specs[@]}"; do
    t=${sp%%:*}; s=${sp##*:}
    make_icon "$s" full "$P_IN_LEGACY" 0 "$TMP/icns/$t.png"
    total=$((total + 8 + $(stat -c%s "$TMP/icns/$t.png")))
  done
  printf 'icns' > "$ICONS/icon.icns"
  be32 "$total" >> "$ICONS/icon.icns"
  for sp in "${specs[@]}"; do
    t=${sp%%:*}; f="$TMP/icns/$t.png"
    printf '%s' "$t" >> "$ICONS/icon.icns"
    be32 "$((8 + $(stat -c%s "$f")))" >> "$ICONS/icon.icns"
    cat "$f" >> "$ICONS/icon.icns"
  done
  echo "[macOS  ] icon.icns（icp4..ic10）"
fi

if ! skipped Ios; then
  # Xcode 的 AppIcon 集：**必须是 appiconset 目录 + Contents.json**，裸 PNG Xcode 根本不认。
  # 同一物理尺寸在不同 idiom 下是**同一个文件**（20x20@2x 的 iPhone 与 iPad 都是 40px），
  # 只有 iPad 独有的槽位才另起 `~ipad` 文件——旧图标里那几个 `@2x-1.png` 是
  # `cargo tauri icon` 自己的 iPad 命名，既不是 Xcode 约定，内容又和 `@2x` 一模一样。
  slots='AppIcon-20x20@1x.png|20|universal|1x|20x20
AppIcon-20x20@2x.png|40|universal|2x|20x20
AppIcon-20x20@3x.png|60|iphone|3x|20x20
AppIcon-20x20@1x~ipad.png|20|ipad|1x|20x20
AppIcon-20x20@2x~ipad.png|40|ipad|2x|20x20
AppIcon-29x29@1x.png|29|universal|1x|29x29
AppIcon-29x29@2x.png|58|universal|2x|29x29
AppIcon-29x29@3x.png|87|iphone|3x|29x29
AppIcon-29x29@1x~ipad.png|29|ipad|1x|29x29
AppIcon-29x29@2x~ipad.png|58|ipad|2x|29x29
AppIcon-40x40@2x.png|80|universal|2x|40x40
AppIcon-40x40@3x.png|120|iphone|3x|40x40
AppIcon-40x40@1x~ipad.png|40|ipad|1x|40x40
AppIcon-40x40@2x~ipad.png|80|ipad|2x|40x40
AppIcon-60x60@2x.png|120|iphone|2x|60x60
AppIcon-60x60@3x.png|180|iphone|3x|60x60
AppIcon-76x76@1x~ipad.png|76|ipad|1x|76x76
AppIcon-76x76@2x~ipad.png|152|ipad|2x|76x76
AppIcon-83.5x83.5@2x~ipad.png|167|ipad|2x|83.5x83.5
AppIcon-1024x1024.png|1024|ios-marketing|1x|1024x1024'

  iosdir="$ICONS/ios/AppIcon.appiconset"
  rm -rf "$iosdir"                 # 先清干净，避免残留旧文件
  mkdir -p "$iosdir"
  : > "$TMP/contents.json"
  first=1
  while IFS='|' read -r fn sz idiom scale slot; do
    [ -z "$fn" ] && continue
    make_icon "$sz" full "$P_IN_LEGACY" 0 "$iosdir/$fn"
    [ $first = 1 ] && first=0 || printf ',\n' >> "$TMP/contents.json"
    printf '    { "filename": "%s", "idiom": "%s", "scale": "%s", "size": "%s" }' \
      "$fn" "$idiom" "$scale" "$slot" >> "$TMP/contents.json"
  done <<< "$slots"
  {
    printf '{\n  "images": [\n'
    cat "$TMP/contents.json"
    printf '\n  ],\n  "info": { "author": "purevox", "version": 1 }\n}\n'
  } > "$iosdir/Contents.json"
  echo "[iOS    ] AppIcon.appiconset（20 个槽位 + Contents.json）"
fi

if ! skipped Tray; then
  make_icon 64 squircle "$P_IN_TRAY" 0 "$ICONS/tray_running.png"
  make_icon 64 squircle "$P_IN_TRAY" 1 "$ICONS/tray_stopped.png"
  echo "[托盘   ] tray_running / tray_stopped（64px，蓝 / 暗灰）"
fi

if ! skipped Android; then
  # 密度:mdpi hdpi xhdpi xxhdpi xxxhdpi — 传统图标边长 / 自适应前景边长
  while read -r d legacy fg; do
    mkdir -p "$RES/mipmap-$d"
    make_icon "$legacy" squircle "$P_IN_LEGACY" 0 "$RES/mipmap-$d/ic_launcher.png"
    make_icon "$legacy" round     "$P_IN_LEGACY" 0 "$RES/mipmap-$d/ic_launcher_round.png"
    make_icon "$fg"     none      "$P_IN_ADAPTIVE" 0 "$RES/mipmap-$d/ic_launcher_foreground.png"
  done <<'DENS'
mdpi 48 108
hdpi 72 162
xhdpi 96 216
xxhdpi 144 324
xxxhdpi 192 432
DENS
  echo "[Android] mipmap 五档 × 3（round 用 4× 超采样，圆边不再是台阶）"
fi

echo "完成。"