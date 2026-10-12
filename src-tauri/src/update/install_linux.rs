// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// The built-in AI models are NOT covered by the GPL; they are the
// property of a2heng and may only be used with PureVox under
// authorization.  See MODEL-LICENSE.md for details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Linux 安装：**只有 AppImage 能自动更新**（直接覆盖正在运行的 AppImage 即可
//! ——AppImage 挂载的是副本，覆盖源文件不影响当前进程，重启即生效）。
//! deb / rpm 由系统包管理器管，程序无权替换自己 → 明确报「不支持 + release 页面」，
//! 不静默失败（AGENTS.md：不静默）。

use super::{Phase, Status};

/// AppImage 优先（可自替换），deb / rpm 只作下载兜底。
pub const ASSET_SUFFIXES: &[&str] = &[".appimage", ".deb", ".rpm"];

/// 是否是 AppImage 安装（`APPIMAGE` 环境变量由 AppImage 运行时设置）。
fn is_appimage() -> bool {
  std::env::var_os("APPIMAGE")
    .map(|v| !v.is_empty())
    .unwrap_or(false)
}

pub fn install(st: &Status) -> Result<String, String> {
  if st.phase != Phase::Ready {
    return Err("还没有下载好安装包".into());
  }
  if !is_appimage() {
    super::with(|s| {
      s.phase = Phase::Unsupported;
      s.reason = "deb / rpm 安装由系统包管理器接管，程序不能自己替换；请下载后安装".into();
    });
    return Err("当前不是 AppImage 安装，无法自动替换".into());
  }
  let new = std::path::Path::new(&st.saved_path);
  if !new.is_file() {
    return Err(format!("安装包不见了：{}", st.saved_path));
  }
  let cur = std::env::var_os("APPIMAGE")
    .map(std::path::PathBuf::from)
    .unwrap();
  // 先写同目录临时名再 rename（同一文件系统内原子替换）
  let tmp = cur.with_extension("new");
  std::fs::copy(new, &tmp).map_err(|e| format!("写入新版本失败：{e}"))?;
  std::fs::set_permissions(
    &tmp,
    std::fs::Permissions::from(std::os::unix::fs::PermissionsExt::from_mode(0o755)),
  )
  .map_err(|e| format!("设置可执行权限失败：{e}"))?;
  std::fs::rename(&tmp, &cur).map_err(|e| {
    let _ = std::fs::remove_file(&tmp);
    format!("替换当前 AppImage 失败：{e}")
  })?;
  super::with(|s| s.phase = Phase::Installing);
  Ok("已替换 AppImage，重启 PureVox 生效".into())
}
