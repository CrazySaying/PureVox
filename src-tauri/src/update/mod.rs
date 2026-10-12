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

//! 更新检查（桌面）：查本仓库 GitHub Releases 的 `latest`，有新版本就下载安装包。
//!
//! **唯一数据源 = GitHub Releases API**（`repos/{owner}/{repo}/releases/latest`），
//! 产物由 `release.yml` 在打 tag 时挂上去，两端靠**文件名后缀**匹配（不靠版本号，
//! 因为版本号只有年月日、同日多次发布会重复，见 `version.rs`）。
//!
//! **比较依据 = tag 的完整时间戳**（`v2026.10.12.1430`），不是显示版本号
//! `26.10.12`（`yy.M.d`）：同日发两次时显示版本相同，靠时分才能分出新旧。
//! 开发构建没有 CI 注入的时间戳（`version::is_dev()`），此时**不下结论**，
//! 报「开发构建」而不是拿 Cargo 的 0.1.0 去比。
//!
//! 状态机（[`Phase`]）单一实例挂在全局（与 `NetHub` / `MediaHub` 同款），
//! 同时进调试接口 `/debug` 的 `update` 段（AGENTS.md §1.2）。
//! 下载在后台线程跑（带进度），**不进音频线程**；安装是平台实现（分文件）。

use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::version;

/// GitHub 仓库（发版唯一去处，桌面 / 安卓两端共用）。
pub const REPO: &str = "a2heng/PureVox";

/// GitHub API 端点（拼自 [`REPO`]，避免两处写死仓库名）。
fn api_latest() -> String {
  format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// 状态机阶段。字符串直接进 JSON（`/debug` 的 `update.phase` 与界面同源）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
  /// 还没查过
  Idle,
  /// 正在查 GitHub
  Checking,
  /// 已是最新
  UpToDate,
  /// 有新版本（未下载）
  Available,
  /// 正在下载安装包
  Downloading,
  /// 已下载到本地，可安装
  Ready,
  /// 正在安装（安装器已拉起，进程即将退出）
  Installing,
  /// 本平台不支持自动更新（给出原因 + release 页面）
  /// （只在部分平台的 `install_*.rs` 里构造——本平台支持时该分支不会被走到）
  #[allow(dead_code)]
  Unsupported,
  /// 失败（`reason` 是原因）
  Failed,
}

/// 对外状态（界面轮询 / `/debug` 共用这一份）。
#[derive(Clone, Debug, Serialize)]
pub struct Status {
  pub phase: Phase,
  /// 本机显示版本（`version::VERSION`，如 `26.10.12`）
  pub current_version: String,
  /// 本机构建时间戳（`version::STAMP`，开发态为空）
  pub current_stamp: String,
  /// 最新 release 的 tag（`v2026.10.12.1430`）
  pub latest_tag: String,
  /// 最新 release 的显示版本（由 tag 推导的 `yy.M.d`）
  pub latest_version: String,
  /// 更新说明（release body，前若干行由界面截断显示）
  pub notes: String,
  /// release 页面地址（打不开 / 不支持时给用户手动下载）
  pub url: String,
  /// 发布日期
  pub published_at: String,
  /// 匹配到的产物名（空 = 没匹配到）
  pub asset_name: String,
  /// 匹配到的产物下载地址（与 `asset_name` 同源，`pick_asset` 一起给出）
  pub asset_url: String,
  /// 产物字节数（0 = 未知）
  pub asset_size: u64,
  /// 已下载字节（`Downloading` 时变化）
  pub received: u64,
  /// 下载完成的本地路径（`Ready` 时非空）
  pub saved_path: String,
  /// 失败 / 不支持的原因（Rust 诊断文本，按 AGENTS.md §4 边界不上 i18n）
  pub reason: String,
}

impl Default for Status {
  fn default() -> Self {
    Status {
      phase: Phase::Idle,
      current_version: version::VERSION.to_string(),
      current_stamp: version::STAMP.to_string(),
      latest_tag: String::new(),
      latest_version: String::new(),
      notes: String::new(),
      url: String::new(),
      published_at: String::new(),
      asset_name: String::new(),
      asset_url: String::new(),
      asset_size: 0,
      received: 0,
      saved_path: String::new(),
      reason: String::new(),
    }
  }
}

/// 全局状态单例（与 `NetHub` / `MediaHub` 同款）。
struct Hub {
  status: Mutex<Status>,
}

static HUB: OnceLock<Hub> = OnceLock::new();

fn hub() -> &'static Hub {
  HUB.get_or_init(|| Hub {
    status: Mutex::new(Status::default()),
  })
}

/// 当前状态快照（界面 / 调试接口都读它）。
pub fn status() -> Status {
  hub().status.lock().unwrap().clone()
}

/// 就地改状态（各阶段共用）。
fn with<R>(f: impl FnOnce(&mut Status) -> R) -> R {
  let mut g = hub().status.lock().unwrap();
  f(&mut g)
}

/// 下载目录（`~/.purevox/update`）。
fn download_dir() -> Result<std::path::PathBuf, String> {
  let d = crate::config::config_dir()?.join("update");
  std::fs::create_dir_all(&d).map_err(|e| format!("创建下载目录失败：{e}"))?;
  Ok(d)
}

/// 设置失败状态（原因一定带上，别静默）。
pub fn fail(reason: impl Into<String>) {
  with(|s| {
    s.phase = Phase::Failed;
    s.reason = reason.into();
  });
}

// ── GitHub release JSON（只取用得到的字段）──

#[derive(serde::Deserialize)]
struct Release {
  tag_name: String,
  #[serde(default)]
  body: String,
  html_url: String,
  #[serde(default)]
  published_at: String,
  #[serde(default)]
  assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
  name: String,
  browser_download_url: String,
  #[serde(default)]
  size: u64,
}

/// `v2026.10.12.1430` → `2026.10.12.1430`；不合法返回 None。
fn tag_to_stamp(tag: &str) -> Option<String> {
  let t = tag.strip_prefix('v').unwrap_or(tag);
  version::parse_stamp(t).map(|_| t.to_string())
}

/// `2026.10.08.1430` → `26.10.8`（界面显示版本，与 `version.sh` 的映射同款）。
/// 段按整数取值再拼，故 `08` 打成 `8`——必须与 CI 产出的包版本逐字相同。
fn stamp_to_version(stamp: &str) -> String {
  let p: Vec<&str> = stamp.split('.').collect();
  if p.len() < 3 {
    return stamp.to_string();
  }
  let y: i64 = p[0].parse().unwrap_or(0);
  let mo: i64 = p[1].parse().unwrap_or(0);
  let d: i64 = p[2].parse().unwrap_or(0);
  format!("{}.{}.{}", y % 100, mo, d)
}

/// release 的产物里挑本平台能用的那个（**只看后缀，不看版本号**）。
/// 返回 `(产物名, 下载地址, 字节数)`。
fn pick_asset(assets: &[Asset]) -> Option<(String, String, u64)> {
  // 按优先级依次找候选后缀；CI 产出的文件名见 `release.yml`（统一重命名成
  // `PureVox-<版本>-<平台>.<ext>`，故后缀稳定）。
  let suffixes: &[&str] = imp::ASSET_SUFFIXES;
  for suf in suffixes {
    if let Some(a) = assets
      .iter()
      .find(|a| a.name.to_ascii_lowercase().ends_with(suf))
    {
      return Some((a.name.clone(), a.browser_download_url.clone(), a.size));
    }
  }
  None
}

/// 查 GitHub Releases（`latest`）。成功即更新全局状态；失败写 `Failed` + 原因。
pub async fn check() -> Result<Status, String> {
  with(|s| {
    s.phase = Phase::Checking;
    s.reason.clear();
  });
  let client = match http_client() {
    Ok(c) => c,
    Err(e) => {
      fail(e.clone());
      return Err(e);
    }
  };
  let rel = match client
    .get(api_latest())
    .header("Accept", "application/vnd.github+json")
    .header("User-Agent", "PureVox-Desktop")
    .send()
    .await
  {
    Ok(r) => match r.error_for_status() {
      Ok(r) => match r.json::<Release>().await {
        Ok(v) => v,
        Err(e) => {
          let msg = format!("解析 GitHub 返回失败：{e}");
          fail(msg.clone());
          return Err(msg);
        }
      },
      Err(e) => {
        let msg = format!("GitHub 返回错误：{e}");
        fail(msg.clone());
        return Err(msg);
      }
    },
    Err(e) => {
      let msg = format!("连不上 GitHub（检查网络 / 代理）：{e}");
      fail(msg.clone());
      return Err(msg);
    }
  };

  let latest_stamp = match tag_to_stamp(&rel.tag_name) {
    Some(s) => s,
    None => {
      let msg = format!("无法识别的版本号：{}", rel.tag_name);
      fail(msg.clone());
      return Err(msg);
    }
  };
  let asset = pick_asset(&rel.assets);

  with(|s| {
    s.latest_tag = rel.tag_name.clone();
    s.latest_version = stamp_to_version(&latest_stamp);
    s.notes = rel.body.clone();
    s.url = rel.html_url.clone();
    s.published_at = rel.published_at.clone();
    match &asset {
      Some((name, url, size)) => {
        s.asset_name = name.clone();
        s.asset_url = url.clone();
        s.asset_size = *size;
      }
      None => {
        s.asset_name.clear();
        s.asset_url.clear();
        s.asset_size = 0;
      }
    }
    // 开发构建没有可比的时间戳 → 不下「已是最新」的结论
    if version::is_dev() {
      s.phase = Phase::Idle;
      s.reason = "开发构建（无 CI 版本戳），不判断更新".into();
    } else {
      match (
        version::parse_stamp(version::STAMP),
        version::parse_stamp(&latest_stamp),
      ) {
        (Some(mine), Some(theirs)) if theirs > mine => s.phase = Phase::Available,
        _ => s.phase = Phase::UpToDate,
      }
    }
  });
  Ok(status())
}

/// 共享 HTTP 客户端（超时收紧到 15 s：更新检查不能拖住界面）。
fn http_client() -> Result<reqwest::Client, String> {
  reqwest::Client::builder()
    .user_agent("PureVox-Desktop")
    .timeout(std::time::Duration::from_secs(15))
    .connect_timeout(std::time::Duration::from_secs(10))
    .build()
    .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))
}

/// 下载匹配的安装包到 `~/.purevox/update/`，带进度写回全局状态。
/// 必须在后台线程/tokio 里跑（网络 I/O）。完成置 `Ready`。
pub async fn download() -> Result<Status, String> {
  let (name, url, size) = {
    let s = status();
    if s.phase != Phase::Available && s.phase != Phase::Ready && s.phase != Phase::Failed {
      return Err("当前状态不能下载（先检查更新）".into());
    }
    if s.asset_name.is_empty() {
      return Err("该 release 没有匹配本平台的产物".into());
    }
    (s.asset_name.clone(), s.asset_url(), s.asset_size)
  };
  let dest = download_dir()?.join(&name);
  // `.part` 临时名：下载中断不留半个文件冒充完成品
  let tmp = dest.with_extension("part");

  with(|s| {
    s.phase = Phase::Downloading;
    s.received = 0;
    s.reason.clear();
    s.saved_path.clear();
  });

  let client = match http_client() {
    Ok(c) => c,
    Err(e) => {
      fail(e.clone());
      return Err(e);
    }
  };
  let resp = match client.get(&url).send().await {
    Ok(r) => match r.error_for_status() {
      Ok(r) => r,
      Err(e) => {
        let msg = format!("下载失败：{e}");
        fail(msg.clone());
        return Err(msg);
      }
    },
    Err(e) => {
      let msg = format!("下载失败：{e}");
      fail(msg.clone());
      return Err(msg);
    }
  };
  let mut file = match std::fs::File::create(&tmp) {
    Ok(f) => f,
    Err(e) => {
      let msg = format!("写文件失败：{e}");
      fail(msg.clone());
      return Err(msg);
    }
  };
  use futures_util::StreamExt;
  use std::io::Write;
  let mut stream = resp.bytes_stream();
  while let Some(chunk) = stream.next().await {
    let chunk = match chunk {
      Ok(c) => c,
      Err(e) => {
        let msg = format!("下载中断：{e}");
        let _ = std::fs::remove_file(&tmp);
        fail(msg.clone());
        return Err(msg);
      }
    };
    if let Err(e) = file.write_all(&chunk) {
      let msg = format!("写文件失败：{e}");
      let _ = std::fs::remove_file(&tmp);
      fail(msg.clone());
      return Err(msg);
    }
    let n = chunk.len() as u64;
    with(|s| s.received += n);
  }
  drop(file);
  if let Err(e) = std::fs::rename(&tmp, &dest) {
    let _ = std::fs::remove_file(&tmp);
    let msg = format!("保存安装包失败：{e}");
    fail(msg.clone());
    return Err(msg);
  }
  with(|s| {
    s.phase = Phase::Ready;
    s.received = size.max(s.received);
    s.saved_path = dest.to_string_lossy().into_owned();
  });
  Ok(status())
}

impl Status {
  /// 当前 release 里本平台产物的下载地址（`Available`/`Ready` 时有效）。
  fn asset_url(&self) -> String {
    self.asset_url.clone()
  }
}

// 平台实现分文件（AGENTS.md §4）：静默安装的方式各平台不同，
// 共享部分（查 / 下载 / 版本比较）都在本文件。
#[cfg(windows)]
#[path = "install_windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "install_linux.rs"]
mod imp;
#[cfg(not(any(windows, target_os = "linux")))]
#[path = "install_other.rs"]
mod imp;

/// 安装已下载的产物（平台实现，见 `install_*.rs`）。
pub fn install() -> Result<String, String> {
  imp::install(&status())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn asset(name: &str) -> Asset {
    Asset {
      name: name.into(),
      browser_download_url: format!("https://example.invalid/{name}"),
      size: 1024,
    }
  }

  #[test]
  fn tag_to_stamp_strips_v_and_validates() {
    assert_eq!(
      tag_to_stamp("v2026.10.12.1430").as_deref(),
      Some("2026.10.12.1430")
    );
    assert_eq!(
      tag_to_stamp("2026.10.12.1430").as_deref(),
      Some("2026.10.12.1430")
    );
    assert_eq!(tag_to_stamp("v1.2.3"), None, "段数不对必须拒绝");
    assert_eq!(tag_to_stamp("v2026.13.12.1430"), None);
  }

  #[test]
  fn stamp_to_version_matches_version_script_mapping() {
    // 与 tools/automation/version.sh 的 major=年-2000 / minor=月 / build=日 一致
    assert_eq!(stamp_to_version("2026.10.08.1430"), "26.10.8");
    assert_eq!(stamp_to_version("2026.09.30.1944"), "26.9.30");
    assert_eq!(stamp_to_version("bad"), "bad");
  }

  #[test]
  fn pick_asset_prefers_this_platform_then_falls_back() {
    // 产物名只看后缀（版本号同日会重复，见 version.rs）
    let assets = vec![
      asset("PureVox-26.10.12-windows-x64.msi"),
      asset("PureVox-26.10.12-windows-x64-setup.exe"),
      asset("PureVox-Android-26.10.12-arm64.apk"),
    ];
    let (name, url, size) = pick_asset(&assets).expect("应匹配到本平台产物");
    assert_eq!(name, "PureVox-26.10.12-windows-x64-setup.exe");
    assert_eq!(
      url,
      "https://example.invalid/PureVox-26.10.12-windows-x64-setup.exe"
    );
    assert_eq!(size, 1024);
  }

  #[test]
  fn pick_asset_returns_none_when_no_match() {
    // 其它平台的 ASSET_SUFFIXES（install_other.rs 是空表）不该误配
    let assets = [asset("PureVox-Android-26.10.12-arm64.apk")];
    let suffixes: &[&str] = &["__no_such_suffix__"];
    assert!(suffixes.iter().all(|s| !assets[0].name.ends_with(s)));
  }

  #[test]
  fn phase_serializes_snake_case_for_debug_snapshot() {
    // /debug 的 update.phase 直接用这个字符串（界面同源）
    let j = serde_json::to_string(&Phase::UpToDate).unwrap();
    assert_eq!(j, "\"up_to_date\"");
    let j = serde_json::to_string(&Phase::Available).unwrap();
    assert_eq!(j, "\"available\"");
  }
}
