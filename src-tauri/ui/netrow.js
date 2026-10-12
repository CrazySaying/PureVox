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

// 网络行（`remote_mic` 网络麦克风 / `remote_speaker` 网络扬声器）的卡片内容。
//
// **为什么要单独一个文件**：网络行不需要选设备，光一个下拉框等于什么都没说——用户
// 建完这行不知道下一步干什么。真正的操作全在这里：服务开关/重启、状态灯、扫码用的
// 网址与二维码、这行到底干什么的说明。与 `media.js`（音效板 / 播放器）同理，由
// 本模块独占这些控件，`columns.js` 不再画第二遍。
//
// 与顶栏「手机」面板读的是同一份状态（`net_status` / `net_running` / `net_local_addr`，
// 都是 `/debug` 的 `net` 段同源）。网络行卡片是**唯一**的手机/客户端操作入口：顶栏
// 不再有独立的「手机」面板（用户 2026-10-12 明确要求——「手机不要单独的面板，就是
// 这个网络面板」），没建网络行时想连客户端就只能先把行加上，这是有意的。

;(function () {
  'use strict'

  /** @param {string} s @param {Record<string, string|number>} [vars] */
  const T = (s, vars) => (window.__pvT ? window.__pvT(s, vars) : s)

  /**
   * @param {string} cmd @param {Record<string, unknown>} [args]
   * @returns {Promise<any>}
   */
  function invoke(cmd, args) {
    if (!window.__TAURI__) return Promise.reject(new Error(T('无 Tauri 运行时')))
    return window.__TAURI__.core.invoke(cmd, args)
  }

  /** 当前地址（`ws://ip:port`），失败给原因。同一份数据给顶栏面板与这里的二维码。 */
  let addrUrl = ''
  /** @type {{text: string, ok: boolean}|null} */
  let lastStatus = null
  let remoteInput = false
  /** 本次挂载里建过的卡片，重绘时统一收摊 */
  /** @type {Set<{el: HTMLElement, paint: () => void}>} */
  const cards = new Set()
  /** @type {number|null} */
  let timer = null

  /**
   * 拉一次状态 + 地址，刷新所有已挂载的网络行卡片。
   * @returns {Promise<void>}
   */
  async function poll() {
    let ok = false
    let text = ''
    try {
      const [s, a] = await Promise.all([
        invoke('net_status'),
        invoke('net_local_addr'),
      ])
      ok = true
      text = String(s)
      addrUrl = a && a.url ? String(a.url) : ''
    } catch (e) {
      text = T('状态不可用：') + String(e)
      addrUrl = ''
    }
    lastStatus = { text, ok }
    for (const c of cards) {
      if (c.el.isConnected) c.paint()
      else cards.delete(c)
    }
  }

  /** @param {HTMLButtonElement} b @param {string} label */
  function busy(b, label) {
    b.disabled = true
    b.dataset.was = b.textContent || ''
    if (label) b.textContent = label
  }

  /**
   * 跑一个服务动作，按钮上给反馈，成功/失败都写进状态行（不静默）。
   * @param {HTMLButtonElement} btn @param {string} cmd @param {string} doing
   * @param {(r: any) => string} [fmt]
   */
  async function action(btn, cmd, doing, fmt) {
    busy(btn, doing)
    try {
      const r = await invoke(cmd)
      if (window.__pvDebug) window.__pvDebug(T('网络服务：') + String(r))
    } catch (e) {
      if (window.__pvDebug) window.__pvDebug(T('网络服务失败：') + String(e))
    } finally {
      btn.disabled = false
      btn.textContent = btn.dataset.was || ''
      await poll()
    }
  }

  /** 复制到剪贴板；失败要说出来（WebView2 的剪贴板在非用户手势里可能被拒）。 */
  async function copy(text, btn) {
    try {
      if (!navigator.clipboard) throw new Error(T('浏览器不支持剪贴板'))
      await navigator.clipboard.writeText(text)
      const old = btn.dataset.was2 || btn.textContent || ''
      btn.dataset.was2 = old
      btn.textContent = T('已复制')
      window.setTimeout(() => {
        btn.textContent = old
      }, 1200)
    } catch (e) {
      if (window.__pvDebug) window.__pvDebug(T('复制失败：') + String(e))
    }
  }

  /**
   * 在一个卡片里画二维码。内容就是本机地址，手机扫了直接打开网页版客户端。
   * @param {HTMLElement} host
   */
  function drawQr(host) {
    const url = addrUrl
    const canvas = /** @type {HTMLCanvasElement|null} */ (host.querySelector('canvas'))
    const note = /** @type {HTMLElement|null} */ (host.querySelector('.netqr-note'))
    if (!canvas || !window.__pvQr) return
    if (!url) {
      // 服务没起来 → 地址拿不到。与其画一张扫不通的码，不如说清楚为什么没码。
      const ctx = canvas.getContext('2d')
      if (ctx) {
        ctx.clearRect(0, 0, canvas.width, canvas.height)
      }
      canvas.classList.add('off')
      if (note) note.textContent = T('服务未启动，没有可扫的地址')
      return
    }
    canvas.classList.remove('off')
    try {
      window.__pvQr.render(url, canvas, { scale: 4, margin: 2 })
      if (note) note.textContent = T('客户端扫码打开网页版客户端')
    } catch (e) {
      canvas.classList.add('off')
      if (note) note.textContent = T('二维码生成失败：') + String(e)
    }
  }

  /**
   * 给一行网络行挂上完整卡片。
   * @param {HTMLElement} elRow 行容器（`columns.js` 传进来）
   * @param {number} _ci @param {number} _ri @param {any} row
   */
  function attach(elRow, _ci, _ri, row) {
    const out = row.ptype === 'remote_speaker'

    // 网络行卡片。**手机相关的全部操作都在这里**——顶栏不再有独立面板（用户明确要求：
    // 「手机不要单独的面板，就是这个网络面板」）。这一行既是链路的一环，也是客户端
    // 的说明书：状态、地址、二维码、重启、远程输入、端口，缺一样用户就接不上。
    //
    // 「手机」这个词只在指代真机时出现；客户端本身可能是手机、平板或浏览器网页版，
    // 所以一律叫「客户端」（见 AGENTS.md §1.2：远程音频与远程输入是**网络**功能）。
    const box = document.createElement('div')
    box.className = 'netcard'

    // 1) 状态灯 + 状态串（与 `/debug` 的 `net` 段同源；失败给原因，不显示伪造的 0）
    const bar = document.createElement('div')
    bar.className = 'netcard-bar'
    const dot = document.createElement('span')
    dot.className = 'netdot'
    const status = document.createElement('span')
    status.className = 'netcard-status'
    bar.append(dot, status)

    const acts = document.createElement('span')
    acts.className = 'netcard-acts'
    /**
     * 按钮文案必须**在调用点**就写 `T('…')` 字面量：i18n 门禁（E1）靠扫描
     * `T('…')` 的字面量收集 msgid，`T(变量)` 它看不见，动态文案也翻译不了。
     * @param {string} text 已翻译的按钮文字
     * @param {string} cmd @param {string} doing 已翻译的进行中文字
     * @param {string} tip 已翻译的 tooltip
     * @param {boolean} [primary]
     */
    const mk = (text, cmd, doing, tip, primary) => {
      const b = document.createElement('button')
      b.type = 'button'
      b.textContent = text
      b.title = tip
      if (primary) b.className = 'primary'
      b.addEventListener('click', () => action(b, cmd, doing))
      acts.appendChild(b)
      return b
    }
    mk(T('重启服务'), 'net_restart', T('重启中'), T('重开监听并重新绑定端口（改端口后用这个生效）'), true)
    bar.appendChild(acts)
    box.appendChild(bar)

    // 2) 客户端地址 + 复制。二维码的内容就是它，两边永远一致（同一个 `addrUrl`）。
    const addrBox = document.createElement('div')
    addrBox.className = 'netcard-addr'
    const lab = document.createElement('span')
    lab.className = 'netcard-label'
    lab.textContent = T('客户端地址')
    const url = document.createElement('span')
    url.className = 'mono netcard-url'
    const copyBtn = document.createElement('button')
    copyBtn.type = 'button'
    copyBtn.className = 'icon'
    copyBtn.textContent = T('复制')
    copyBtn.title = T('复制客户端地址')
    copyBtn.addEventListener('click', () => copy(addrUrl, copyBtn))
    addrBox.append(lab, url, copyBtn)
    box.appendChild(addrBox)

    // 3) 二维码在左，**说明文字在二维码右边**（一栏竖排，不散在卡片各处）。
    //    下面再放远程输入开关——高度由内容决定，不留空白。
    const qrBox = document.createElement('div')
    qrBox.className = 'netqr'
    const canvas = document.createElement('canvas')
    canvas.className = 'netqr-canvas'
    canvas.setAttribute('role', 'img')
    const side = document.createElement('div')
    side.className = 'netqr-side'
    const note = document.createElement('div')
    note.className = 'netqr-note'
    const help = document.createElement('div')
    help.className = 'netcard-help'
    help.textContent = out
      ? T('网络扬声器：把处理后的声音 Opus 编码后发给所有已订阅的客户端。客户端连上并订阅后就有声音。')
      : T('网络麦克风：远端设备的麦克风当电脑麦克风用，照样进降噪 / TSE / AEC 全链。客户端需连接上面的地址。')
    side.append(note, help)
    qrBox.append(canvas, side)
    box.appendChild(qrBox)

    // 5) 远程输入开关：打字 + 全尺寸键盘。默认关闭（局域网无鉴权，AGENTS.md §1.2）。
    const ri = document.createElement('label')
    ri.className = 'en netcard-remote'
    const cb = document.createElement('input')
    cb.type = 'checkbox'
    cb.addEventListener('change', () => {
      invoke('net_set_remote_input', { on: cb.checked })
        .then(() => {
          remoteInput = cb.checked
        })
        .catch((e) => {
          cb.checked = remoteInput // 失败必须回滚，不能界面显示已开而实际是关
          if (window.__pvDebug) window.__pvDebug(T('远程输入开关失败：') + String(e))
        })
    })
    ri.append(cb, document.createTextNode(T('远程输入（打字 + 键盘，默认关闭）')))
    ri.title = T('同网段无鉴权：开启后同一 Wi-Fi 下的任何设备都能向这台电脑打字和发按键')
    box.appendChild(ri)

    /** 重绘状态相关的部分（2 Hz，AGENTS.md §1.1：不卡 UI） */
    const paint = () => {
      const s = lastStatus
      if (s) {
        status.textContent = s.text
        dot.className = 'netdot ' + (s.ok ? 'ok' : 'na')
      }
      url.textContent = addrUrl || T('服务未启动')
      url.className = 'mono netcard-url ' + (addrUrl ? '' : 'na')
      drawQr(qrBox)
    }

    elRow.appendChild(box)
    cards.add({ el: elRow, paint })
    paint()
    if (timer === null) timer = window.setInterval(poll, 500)
    return true
  }

  /** 重绘整张计划前调用（`columns.js` 的 render 会重建 DOM） */
  function reset() {
    cards.clear()
    if (timer !== null) {
      window.clearInterval(timer)
      timer = null
    }
  }

  window.addEventListener('DOMContentLoaded', () => {
    poll()
    document.addEventListener('pv-langchange', () => {
      for (const c of cards) c.paint()
    })
    window.addEventListener('beforeunload', reset)
  })

  window.__pvNetRow = { attach, reset }
})()