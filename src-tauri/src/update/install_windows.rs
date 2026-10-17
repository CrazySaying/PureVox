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

//! Windows 静默安装：NSIS 安装器的 `/S /UPDATE /NS /R`（见 Tauri 的
//! `installer.nsi`：`/S` 静默、`/UPDATE` 跳过重装页与快捷方式、`/NS` 不建快捷方式、
//! `/R` 装完重启应用）。
//!
//! **Windows 只发 NSIS 一个安装包**（`-setup.exe`），故这里没有第二个分支：
//! `src-tauri/tauri.windows.conf.json` 把 `bundle.targets` 收成 `["nsis"]`，
//! 不再产出 MSI（一个平台一条安装路径，见 AGENTS §2.1）。少一种产物 = 少一种
//! 「更新时挑错包」的可能，也不用再维护 msiexec 分支。
//!
//! **先退进程再装**：NSIS 安装器开头有 `CheckIfAppIsRunning`，正在运行的
//! `purevox.exe` 会被它拦下弹框。这里用一个**脱离的 cmd**先歇几秒等本进程退出，
//! 再拉安装器；随后调用方 `app.exit(0)`，安装器接着覆盖文件并按 `/R` 重启。

use super::{Phase, Status};

/// 本平台在 release 产物里认的后缀：**只有 NSIS**（`tauri.windows.conf.json` 只出它）。
pub const ASSET_SUFFIXES: &[&str] = &["-setup.exe"];

/// 拉起安装器并返回一句给界面看的话；调用方随后应退出进程。
pub fn install(st: &Status) -> Result<String, String> {
  if st.phase != Phase::Ready {
    return Err("还没有下载好安装包".into());
  }
  let exe = std::path::Path::new(&st.saved_path);
  if !exe.is_file() {
    return Err(format!("安装包不见了：{}", st.saved_path));
  }
  let cmdline = format!(
    "ping -n 5 127.0.0.1 >nul & \"{}\" /S /UPDATE /NS /R",
    exe.display()
  );
  use std::os::windows::process::CommandExt;
  const DETACHED_PROCESS: u32 = 0x0000_0008;
  const CREATE_NO_WINDOW: u32 = 0x0800_0000;
  std::process::Command::new("cmd")
    .args(["/C", &cmdline])
    .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
    .spawn()
    .map_err(|e| format!("拉起安装器失败：{e}"))?;
  super::with(|s| s.phase = Phase::Installing);
  Ok("安装器已启动，正在退出并更新…".into())
}
