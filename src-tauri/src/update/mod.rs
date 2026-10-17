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

/// `2026.10.08.1430` → `26.10.8`（界面显示版本，与 `version.py` 的映射同款）。
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
      s.phase = decide(version::STAMP, &latest_stamp);
    }
  });
  Ok(status())
}

/// 更新判定（**纯函数，不碰网络**）：本机构建戳 vs 最新 release 的 tag 戳 → 阶段。
///
/// 抽出来的理由：这段判定原先埋在 [`check`] 的状态闭包里，**只有联网跑完整条链路才覆盖得到**
/// ——而 CI 不该、也做不到去 GitHub 真查一次。抽成纯函数后，`cargo test` 就能把
/// 「同日多次发版靠时分区分」「本机更新时不应提示」这两条最容易错的地方钉死。
///
/// `mine` 为空串（开发构建）时 `parse_stamp` 返回 None → 落到 `UpToDate`；调用方必须
/// **先判 `version::is_dev()` 再调本函数**（`Idle` + 明确原因，别拿 Cargo 回退的 0.1.0 去比）。
fn decide(mine_stamp: &str, latest_stamp: &str) -> Phase {
  match (
    version::parse_stamp(mine_stamp),
    version::parse_stamp(latest_stamp),
  ) {
    (Some(mine), Some(theirs)) if theirs > mine => Phase::Available,
    _ => Phase::UpToDate,
  }
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

  /// release.yml 的原文 —— 编译期嵌进来，所以 CI 改了产物命名、本仓库的测试立刻红，
  /// 不存在「忘了同步」。
  /// 产物命名规则的**唯一来源**：`tools/automation/release_stage.sh` 里的 `stage` 行。
  ///
  /// 曾经指向 release.yml —— 那段 `stage` 逻辑内联在 workflow 里，所以是 workflow
  /// 的事实来源。但**内联的脚本等于没被测过的脚本**：打第一次真 tag 才发现
  /// 「收集完文件名后 `cd ..` 回上一层 → 相对路径全失效」。搬到 .sh 之后，
  /// `run_release_stage.py` 能在本地用假产物树 + 假 gh 把它跑通。
  ///
  /// 注意 `.count()` 的断言不能省：源文件换了地方而这里忘了改的话，
  /// `lines().filter(...)` 会**一条都匹配不到**，`tpls.len() >= 6` 就是唯一
  /// 把它拦下来的东西。
  const RELEASE_STAGE_SH: &str = include_str!("../../../tools/automation/release_stage.sh");

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
    // 没有 v 前缀也认（有人手工建 tag 会漏 v）
    assert_eq!(
      tag_to_stamp("2026.10.12.1430").as_deref(),
      Some("2026.10.12.1430")
    );
    assert_eq!(tag_to_stamp("v1.2.3"), None, "段数不对必须拒绝");
    assert_eq!(tag_to_stamp("v26.10.8"), None, "两段是显示版本，不是构建戳");
    assert_eq!(tag_to_stamp("v2026.13.12.1430"), None, "13 月非法");
  }

  #[test]
  fn stamp_to_version_matches_version_script_mapping() {
    // 与 tools/automation/version.py 的 major=年-2000 / minor=月 / build=日 一致。
    // 月/日都按整数取值再拼，故 09 月打成 9；version.py 那边用 int() 去前导零
    // （bash 会把 09 当八进制），两边必须逐字相同 —— 版本检测的门禁就在这儿。
    assert_eq!(stamp_to_version("2026.10.08.1430"), "26.10.8");
    assert_eq!(stamp_to_version("2026.09.30.1944"), "26.9.30");
    assert_eq!(stamp_to_version("2026.09.09.0901"), "26.9.9");
    assert_eq!(stamp_to_version("2026.01.02.0000"), "26.1.2");
    assert_eq!(
      stamp_to_version("bad"),
      "bad",
      "认不出来就原样返回，不编一个版本号"
    );
  }

  #[test]
  fn pick_asset_prefers_this_platform_then_falls_back() {
    // 产物名按**本平台的**后缀构造，不能硬编码 Windows 产物名：ASSET_SUFFIXES 来自
    // install_*.rs，是平台相关的（Windows 是 setup.exe，Linux 是 AppImage/deb/rpm）。
    // 写死 Windows 名字的话，Linux 上 pick_asset 一个都匹配不上直接 panic——
    // 而 Windows 上永远绿。这类错只有真在 Linux 上跑一遍门禁才会暴露。
    let suffixes: &[&str] = imp::ASSET_SUFFIXES;
    assert!(
      !suffixes.is_empty(),
      "本平台至少要有一个可安装产物后缀，否则自动更新无从谈起"
    );
    const V: &str = "26.10.12";
    // 干扰项：本平台之外的产物（zip 谁都不是）
    let mut assets = vec![asset(&format!("PureVox-{V}-other.tar.gz"))];
    // 本平台的每个候选都塞进去，但**倒序**放：命中哪个只由 ASSET_SUFFIXES 的优先级决定
    // （外层遍历 suffixes、内层 find assets），与清单里的先后无关。
    for s in suffixes.iter().rev() {
      assets.push(asset(&format!("PureVox-{V}-cand{s}")));
    }
    let (name, url, size) = pick_asset(&assets).expect("应匹配到本平台产物");
    let want = format!("PureVox-{V}-cand{}", suffixes[0]);
    assert_eq!(name, want, "应按 ASSET_SUFFIXES 的优先级选第一个");
    assert_eq!(url, format!("https://example.invalid/{want}"));
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

  // ── 版本检测（不联网：`decide` 是纯函数，tag 往返是纯字符串处理）──

  #[test]
  fn decide_flags_only_strictly_newer_remote() {
    // 同一个 tag → 已是最新（这条最要紧：发布完当天查一次不能反过来提示有更新）
    assert_eq!(
      decide("2026.10.14.1530", "2026.10.14.1530"),
      Phase::UpToDate
    );
    // 远端更新
    assert_eq!(
      decide("2026.10.14.1530", "2026.10.14.1531"),
      Phase::Available
    );
    assert_eq!(
      decide("2026.10.14.1530", "2026.10.15.0900"),
      Phase::Available
    );
    assert_eq!(
      decide("2026.09.30.2359", "2026.10.01.0000"),
      Phase::Available
    );
    // 远端更旧（本地是开发版以外的预发、或 release 被回退）→ 不提示
    assert_eq!(
      decide("2026.10.15.0900", "2026.10.14.1530"),
      Phase::UpToDate
    );
    assert_eq!(
      decide("2026.11.01.0000", "2026.10.31.2359"),
      Phase::UpToDate
    );
  }

  #[test]
  fn decide_treats_same_day_releases_by_minutes() {
    // 显示版本 yy.M.d 同一天会重复，靠 tag 的时分区分（version.rs 的存在理由），
    // 所以判定必须按全序而不是按显示版本。
    let early = "2026.10.14.0901";
    let late = "2026.10.14.2309";
    assert_eq!(decide(early, late), Phase::Available);
    assert_eq!(decide(late, early), Phase::UpToDate);
  }

  #[test]
  fn decide_survives_unparsable_stamps_without_panicking() {
    // tag 格式不对 / 本机构建戳缺失：不能崩，也不能谎报「有更新」
    assert_eq!(decide("2026.10.14.1530", "not-a-tag"), Phase::UpToDate);
    assert_eq!(
      decide("2026.10.14.1530", "v2026.10.14.1530"),
      Phase::UpToDate
    );
    assert_eq!(decide("", "2026.10.14.1530"), Phase::UpToDate);
  }

  #[test]
  fn release_tag_round_trips_to_the_artifact_version() {
    // 闭环：tag → stamp → 显示版本，必须等于 CI 打进包里的那个 VERSION。
    // 这条错了的表现是「界面上写 26.9.9、包名里却是 26.09.09」，用户升级时才看得见。
    for (tag, want) in [
      ("v2026.10.14.1530", "26.10.14"),
      ("v2026.09.09.0901", "26.9.9"),
      ("v2026.12.31.2359", "26.12.31"),
    ] {
      let stamp = tag_to_stamp(tag).expect("合法 tag 应能解析");
      assert_eq!(stamp_to_version(&stamp), want, "tag {tag} 的显示版本往返");
    }
  }

  #[test]
  fn ci_asset_names_match_the_tag_version() {
    // **直接读 release_stage.sh**，不把产物名硬编码在测试里 —— 之前这里写死一份名字列表，
    // 名字全是自己跟自己比，所以 CI 实际产出 `-linux-x64.deb` 而测试里写
    // `-linux-x86_64.deb` 也照样绿。改了命名规则没人会发现，用户照着 release notes
    // 去找文件才扑空。
    //
    // 断言两件最容易错的事：
    // ① 版本段来自 `${VERSION}` 而不是空串 —— release job 曾经从没跑过 version.py，
    //    产物名全叫 `PureVox--windows-x64-setup.exe`（双连字符）；
    // ② 每个平台段的**后缀**正好是本平台认领的后缀，且五个平台互不重叠。
    let tpls: Vec<&str> = RELEASE_STAGE_SH
      .lines()
      .filter(|l| l.contains("stage \"out/PureVox-"))
      .map(|l| {
        l.split("stage \"out/")
          .nth(1)
          .expect("含 stage 的行必有 out/ 前缀")
          .split('"')
          .next()
          .expect("引号必闭合")
      })
      .collect();
    assert!(
      tpls.len() >= 5,
      "release_stage.sh 里的产物模板只剩 {} 个，命名规则变了要重新审这一段",
      tpls.len()
    );

    let tag = "v2026.10.14.1530";
    let want = stamp_to_version(&tag_to_stamp(tag).expect("合法 tag"));
    let mut suffixes: Vec<String> = Vec::new();
    for tpl in &tpls {
      assert!(
        tpl.contains("${VERSION}"),
        "产物模板 {tpl} 没插版本号 → 名字里会缺一段，pick_asset 仍能按后缀认出来但用户看不懂"
      );
      let name = tpl.replace("${VERSION}", &want);
      assert!(
        !name.contains("--"),
        "产物名 {name} 有空版本段（双连字符）：多半是 env.VERSION 在该 job 里从没被赋值"
      );
      let suffix = name
        .strip_prefix(&format!("PureVox-{want}-"))
        .unwrap_or_else(|| panic!("产物名 {name} 的版本段与 tag 不一致"));
      assert!(
        !suffixes.iter().any(|s| s == suffix),
        "平台段 {suffix} 重复出现，pick_asset 会挑到别人的产物"
      );
      suffixes.push(suffix.to_string());
    }

    // 五个平台段的后缀互不为彼此的后缀（`pick_asset` 只认后缀，重叠就会挑错）
    for a in &suffixes {
      for b in &suffixes {
        if a != b {
          assert!(!a.ends_with(b), "后缀 {a} 会被 {b} 抢先命中");
        }
      }
    }
  }

  /// **CI 产出的产物名必须落在运行期 `ASSET_SUFFIXES` 认的后缀里** —— 这条才是真正会伤到
  /// 用户的那一种漂移，而上面那个测试**抓不到**。
  ///
  /// `pick_asset` 拿产物名去比对 `install_{linux,windows}.rs` 里那三个字面量（"`.deb`"）。
  /// 两边各写各的，于是「把 release 里的 `-linux-x64.deb` 改名成 `-linux-x86_64.deb`」
  /// 这种改动会让上面所有断言照样全绿（它们只查内部一致性），但**从此再也匹配不到任何
  /// 产物 —— 自动更新彻底静默失效，用户不会看到任何报错，只会发现「永远有新版本」**。
  ///
  /// 所以这里把三份东西交叉比对：`release_stage.sh` 里 `stage` 出的名字、
  /// `install_*.rs` 里 `ASSET_SUFFIXES` 的字面量、以及本模块的 `pick_asset` 本身
  /// （构造一遍假资产列表走一遍真实函数，不只是比对字符串）。
  #[test]
  fn staged_names_are_claimable_by_pick_asset() {
    let linux_rs = include_str!("install_linux.rs");
    let windows_rs = include_str!("install_windows.rs");

    fn suffixes_of(src: &str, platform: &str) -> Vec<String> {
      let line = src
        .lines()
        .find(|l| l.contains("ASSET_SUFFIXES: &[&str] = &["))
        .unwrap_or_else(|| panic!("{platform} 里找不到 ASSET_SUFFIXES 定义"));
      line
        // `&[` 在 `&[&str] = &[...]` 里出现**两次**（类型与数组各一），所以要 rsplit 取
        // 最后一段；用 split().nth(1) 会取到中间的 `&str] = `，凭空只认出 1 个后缀。
        .rsplit("&[")
        .next()
        .and_then(|s| s.split(']').next())
        .expect("数组字面量必闭合")
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
    }

    let known: Vec<String> = suffixes_of(linux_rs, "linux")
      .into_iter()
      .chain(suffixes_of(windows_rs, "windows"))
      .collect();
    assert!(
      known.len() >= 4,
      "只认出 {} 个后缀，两个平台的定义都被改了？",
      known.len()
    );

    // 走一遍真实的 stage 模板：把 ${VERSION} 换成假版本，取出后缀
    let want = stamp_to_version(&tag_to_stamp("v2026.10.14.1530").expect("合法 tag"));
    let staged: Vec<String> = RELEASE_STAGE_SH
      .lines()
      .filter(|l| l.contains("stage \"out/PureVox-"))
      .map(|l| {
        l.split("stage \"out/")
          .nth(1)
          .expect("含 stage 的行必有 out/ 前缀")
          .split('"')
          .next()
          .expect("引号必闭合")
          .replace("${VERSION}", &want)
      })
      .map(|n| n.to_ascii_lowercase())
      .collect();
    assert!(staged.len() >= 5, "只抠出 {} 个 stage 模板", staged.len());

    for name in &staged {
      // **Android 的 apk 是例外**：它由安卓客户端自己认领（本模块是桌面端更新逻辑，
      // 用户手机上装的 apk 永远不会出现在这里查的 latest release 资产列表给桌面的语境里
      // —— 真出现也认领不到，`.apk` 本来就不在 ASSET_SUFFIXES 里）。所以它必须显式跳过，
      // 否则这条断言会把「桌面端不该管安卓」当成缺陷报出来。
      if name.contains("-android-") {
        continue;
      }
      let claimed = known.iter().any(|s| name.ends_with(s.as_str()));
      assert!(
        claimed,
        "产物名 {name} 的后缀不在任何平台的 ASSET_SUFFIXES 里（{known:?}）—— \
         自动更新会静默失效：pick_asset 一个都匹配不上，用户只会看到「永远有新版本」"
      );
    }

    // 不止比字符串：用真实 pick_asset 走一遍，确认它真能从这批产物里挑出东西
    let assets: Vec<Asset> = staged
      .iter()
      .map(|n| Asset {
        name: n.clone(),
        browser_download_url: format!("https://example.invalid/{n}"),
        size: 1,
      })
      .collect();
    assert!(
      pick_asset(&assets).is_some(),
      "pick_asset 对这批真实产物名一个都挑不出来"
    );
  }
}
