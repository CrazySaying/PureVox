/*
 * PureVox — AI 麦克风降噪工具
 * Copyright (C) 2024-2026 a2heng <752848283@qq.com>
 *
 * PureVox is licensed under the GNU General Public License v3.0 or
 * later (GPL-3.0-or-later).  See LICENSE for details.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * The built-in AI models are NOT covered by the GPL; they are the
 * property of a2heng and may only be used with PureVox under
 * authorization.  See MODEL-LICENSE.md for details.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

// 顶栏「更新」面板（状态机在 `src/update/`，数据源是本仓库 GitHub Releases）。
//
// 三步：**检查 → 下载 → 安装**，每一步只在前一步的状态下可点；进度与失败原因
// 直接显示（Rust 侧 reason 是诊断文本，按 AGENTS.md §4 边界不上 i18n）。
// 面板打开时顺手查一次，关着时不打扰；`update_status` 2 Hz 轮询刷新下载进度。

;(function () {
  'use strict'
  const { invoke } = window.__TAURI__.core
  const T = (s, vars) => (window.__pvT ? window.__pvT(s, vars) : s)
  const $ = (id) => document.getElementById(id)

  /** @type {any} */
  let st = null
  /** @type {number|null} */
  let timer = null

  /** 阶段 → 界面文字。key 必须内联在 T() 里（i18n_lint 才查得到缺键）。 */
  const PHASE_TEXT = {
    idle: () => T('尚未检查'),
    checking: () => T('正在检查…'),
    uptodate: () => T('已是最新'),
    available: () => T('有新版本'),
    downloading: () => T('正在下载…'),
    ready: () => T('已下载，可以安装'),
    installing: () => T('正在安装…'),
    unsupported: () => T('本平台不支持自动更新'),
    failed: () => T('失败'),
  }

  const mb = (n) => (n >= 1048576 ? (n / 1048576).toFixed(1) + ' MB' : Math.round(n / 1024) + ' KB')

  function paint() {
    const cur = $('up-cur')
    if (cur) cur.textContent = st ? st.current_version + (st.current_stamp ? '（' + st.current_stamp + '）' : '') : ''
    const latest = $('up-latest')
    if (latest) latest.textContent = st && st.latest_tag ? st.latest_version + '（' + st.latest_tag + '）' : '—'

    const state = $('up-state')
    if (state && st) {
      const txt = (PHASE_TEXT[st.phase] ? PHASE_TEXT[st.phase]() : st.phase) +
        (st.reason ? '：' + st.reason : '')
      state.textContent = txt
      state.className = st.phase === 'failed' || st.phase === 'unsupported' ? 'na'
        : st.phase === 'available' || st.phase === 'ready' ? 'ok' : ''
    }

    const notes = $('up-notes')
    if (notes) {
      // release body 可能很长，只取前几行（GitHub release 说明惯例是首行当标题）
      notes.textContent = st && st.notes
        ? String(st.notes).split('\n').slice(0, 8).join('\n').slice(0, 600)
        : ''
    }

    // 按钮可用性：严格按状态机（不允许跳步，避免对着错误产物操作）
    const busy = st && (st.phase === 'checking' || st.phase === 'downloading')
    const bCheck = /** @type {HTMLButtonElement|null} */ ($('up-check'))
    const bDl = /** @type {HTMLButtonElement|null} */ ($('up-download'))
    const bIn = /** @type {HTMLButtonElement|null} */ ($('up-install'))
    const bPage = /** @type {HTMLButtonElement|null} */ ($('up-page'))
    if (bCheck) bCheck.disabled = !!busy
    if (bDl) bDl.disabled = !!busy || !st || (st.phase !== 'available' && st.phase !== 'failed' && st.phase !== 'ready')
    if (bIn) bIn.disabled = !st || st.phase !== 'ready'
    if (bPage) bPage.disabled = !st || !st.url
  }

  async function refresh() {
    try {
      st = /** @type {any} */ (await invoke('update_status'))
      paint()
    } catch (e) {
      if (window.__pvDebug) window.__pvDebug(T('读取更新状态失败：') + e)
    }
  }

  function openPanel() {
    $('update')?.classList.remove('hidden')
    refresh().then(() => check())
  }
  function closePanel() {
    $('update')?.classList.add('hidden')
  }

  function check() {
    invoke('update_check')
      .then((s) => {
        st = s
        paint()
      })
      .catch((e) => {
        window.__pvDebug?.('检查更新失败：' + e)
        refresh()
      })
  }

  function download() {
    invoke('update_download')
      .then((s) => {
        st = s
        paint()
      })
      .catch((e) => {
        window.__pvDebug?.('下载失败：' + e)
        refresh()
      })
  }

  function install() {
    invoke('update_install')
      .then(() => {
        // 安装器接管，进程很快退出；这里不必再刷状态
        window.__pvDebug?.('已拉起安装器')
      })
      .catch((e) => {
        window.__pvDebug?.('安装失败：' + e)
        refresh()
      })
  }

  function openPage() {
    if (st && st.url) invoke('update_open_page', { url: st.url }).catch(() => {})
  }

  window.addEventListener('DOMContentLoaded', () => {
    $('btn-update')?.addEventListener('click', openPanel)
    $('up-close')?.addEventListener('click', closePanel)
    $('up-check')?.addEventListener('click', check)
    $('up-download')?.addEventListener('click', download)
    $('up-install')?.addEventListener('click', install)
    $('up-page')?.addEventListener('click', openPage)
    refresh()
    timer = window.setInterval(refresh, 500)
    // 语言切换：阶段文字与按钮标签是动态写入的，按当前语言重放
    document.addEventListener('pv-langchange', () => {
      paint()
      refresh()
    })
    window.addEventListener('beforeunload', () => {
      if (timer !== null) window.clearInterval(timer)
    })
  })
})()