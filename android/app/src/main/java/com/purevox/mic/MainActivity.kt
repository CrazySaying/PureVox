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

package com.purevox.mic

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.TextWatcher
import android.util.Log
import android.view.KeyEvent
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.SwitchCompat
import androidx.core.content.ContextCompat
import com.purevox.mic.audio.MicUplink
import com.purevox.mic.audio.OpusSupport
import com.purevox.mic.audio.SpeakerDownlink
import com.purevox.mic.net.Discovery
import com.purevox.mic.net.HealthProbe
import com.purevox.mic.net.WsClient
import com.purevox.mic.service.CaptureService

/**
 * 唯一的界面与编排者（单 Activity，Material 风格）。
 *
 * 它负责把三条独立能力接上电脑端的同一个 WebSocket：
 * 1. 手机麦克风 → 电脑（[MicUplink]）
 * 2. 电脑音频 → 手机扬声器（[SpeakerDownlink]）
 * 3. 远程输入：IME 已组合文本（`text`）+ 手机实体键（`key`）。电脑端**默认关闭**，
 *    没开开关时电脑会回 `err`，本界面把 `err` 原文显示出来，不静默丢弃。
 *
 * 纪律：
 * - **收到 `ready` 之前不推流、不播放**（[applyState] 里 `ready` 是必要条件）。
 * - 状态区是固定行数的读数（连接状态 / RTT / 收发包数 / 播放水位 / Opus 探测），
 *   2 Hz 刷新；失败项显示「不可用 + 原因」，绝不显示伪造的 0。
 * - 所有 OkHttp 回调都切回主线程再动 UI。
 */
class MainActivity : AppCompatActivity() {

    companion object {
        private const val TAG = "PureVoxUI"
        private const val PREFS = "purevox"
        private const val DEFAULT_HOST = "192.168.1.100"
        private const val MAX_LOG_LINES = 80
        private const val STATUS_INTERVAL_MS = 500L

        /**
         * 不转发给电脑的系统键：这些键在本机有系统级语义，转走会让手机用不了（返回/主屏/音量…）。
         * 电脑端按 `keymap.rs` 映射的只是普通键区 + 功能键。
         */
        private val SYSTEM_KEYS = setOf(
            KeyEvent.KEYCODE_HOME,
            KeyEvent.KEYCODE_BACK,
            KeyEvent.KEYCODE_CALL,
            KeyEvent.KEYCODE_ENDCALL,
            KeyEvent.KEYCODE_APP_SWITCH,
            KeyEvent.KEYCODE_POWER,
            KeyEvent.KEYCODE_CAMERA,
            KeyEvent.KEYCODE_FOCUS,
            KeyEvent.KEYCODE_VOLUME_UP,
            KeyEvent.KEYCODE_VOLUME_DOWN,
            KeyEvent.KEYCODE_VOLUME_MUTE,
            KeyEvent.KEYCODE_HEADSETHOOK,
            KeyEvent.KEYCODE_NOTIFICATION,
            KeyEvent.KEYCODE_MENU,
            KeyEvent.KEYCODE_SEARCH,
        )
    }

    private val ui = Handler(Looper.getMainLooper())

    private var ws: WsClient? = null
    private var uplink: MicUplink? = null
    private var downlink: SpeakerDownlink? = null

    private var host = DEFAULT_HOST

    /**
     * 电脑端的端口。**独立于 host 存**，因为它是用户可改的（DESIGN.md §4.1.1），
     * 而且改的是**电脑端**那个设置 —— 手机这边只能从地址里学（mDNS / 二维码），
     * 学到的必须记住，否则下次启动退回 59123 就「莫名连不上」。
     */
    private var port = Proto.PORT
    private var connState = "未连接"
    private var connecting = false
    private var lastErr = ""
    private var opusDetail = ""
    private var rttMs = Double.NaN
    private var healthText = "未自检"

    @Volatile
    private var ready = false

    @Volatile
    private var speakerOn = false

    @Volatile
    private var remoteTextOn = false

    @Volatile
    private var remoteKeysOn = false

    private var suppressTextSync = false
    private var applyingState = false

    /** 局域网自动发现。发现到就自动填地址并连上；找不到才让用户手动填。 */
    private var discovery: Discovery? = null
    /** 已通过 mDNS 发现到电脑（此时地址框只是「看一眼」，不是主路径）。 */
    private var found = false
    /** 发现到的协议版本；与本客户端不一致时明确告诉用户「客户端太旧」。 */
    private var foundProto: Int? = null

    // 界面控件
    private lateinit var etHost: EditText
    private lateinit var btnConnect: Button
    private lateinit var btnHealth: Button
    private lateinit var btnRediscover: Button
    private lateinit var btnReconnect: Button
    private lateinit var tvStatus: TextView
    private lateinit var tvHealth: TextView
    private lateinit var tvErr: TextView
    private lateinit var swMic: SwitchCompat
    private lateinit var swSpeaker: SwitchCompat
    private lateinit var swRemote: SwitchCompat
    private lateinit var swRemoteText: SwitchCompat
    private lateinit var swRemoteKeys: SwitchCompat
    private lateinit var etText: EditText
    private lateinit var tvLog: TextView

    private val ticker = object : Runnable {
        override fun run() {
            updateStatus()
            ui.postDelayed(this, STATUS_INTERVAL_MS)
        }
    }

    private val permissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            applyState()
            updateStatus()
        }

    // ---------------------------------------------------------------- 生命周期

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        bindViews()
        restorePrefs()

        opusDetail = OpusSupport.probe().detail
        if (opusDetail.isEmpty()) opusDetail = "未探测到 audio/opus 编解码器"

        btnConnect.setOnClickListener {
            if (ws == null && !connecting) connect() else disconnect()
        }
        btnHealth.setOnClickListener { runHealthCheck() }
        // 换 Wi-Fi / 刚才没搜到 → 重新发现（停掉再 discover，不复用旧监听）
        btnRediscover.setOnClickListener { rediscover() }
        // 只是掉线、服务还在 → 只重连 WebSocket，不必重新发现（那要等好几秒）
        btnReconnect.setOnClickListener { reconnect() }

        // 三条路并存，谁先成谁上（DESIGN.md §4.1.1）：
        //
        //   1. **缓存** —— 上次连过的那台。手机和电脑通常在同一网络环境（同一个 Wi-Fi），
        //      地址根本没变，用户**不该每次都等发现**。绝大多数情况走这条，秒连。
        //   2. **mDNS** —— 换了 Wi-Fi / 手机热点 / 电脑 DHCP 换了地址，缓存失效时
        //      自动找回新地址。它**同时常开着**（不是「找到就停」），所以缓存先连上、
        //      发现结果和它一致时不会重复连。
        //   3. **扫码 / 手填** —— 首次配对、换电脑、或组播被交换机挡了（AP 隔离）。
        //
        // 三者不是三条实现，是**同一个「连上电脑」动作的三种拿地址方式**（AGENTS §2.1）。
        startDiscovery()
        // 缓存地址非空且不是出厂默认值（= 说明用户之前真的连过）就直接连，不问
        if (host.isNotEmpty() && host != DEFAULT_HOST) {
            log(getString(R.string.msg_reuse_cached, host))
            connect()
        }

        swMic.setOnCheckedChangeListener { _, _ ->
            applyState()
            updateStatus()
        }
        swSpeaker.setOnCheckedChangeListener { _, _ ->
            applyState()
            updateStatus()
        }
        swRemote.setOnCheckedChangeListener { _, _ ->
            refreshRemoteSwitches()
            applyState()
            updateStatus()
        }
        swRemoteText.setOnCheckedChangeListener { _, _ ->
            applyState()
            updateStatus()
        }
        swRemoteKeys.setOnCheckedChangeListener { _, _ ->
            applyState()
            updateStatus()
        }

        etText.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}

            override fun afterTextChanged(s: Editable?) {
                if (suppressTextSync) return
                val typed = s?.toString().orEmpty()
                if (typed.isEmpty()) return
                if (!remoteTextOn || !ready) return
                // 同步给电脑后清空：既让电脑拿到「已组合好」的文本，又不与本地输入打架
                suppressTextSync = true
                try {
                    sendText(typed)
                } finally {
                    etText.setText("")
                    suppressTextSync = false
                }
            }
        })

        requestPermissions()
        refreshRemoteSwitches()
        log("PureVox 客户端就绪 · $opusDetail")
    }

    override fun onStart() {
        super.onStart()
        ui.post(ticker)
    }

    override fun onStop() {
        ui.removeCallbacks(ticker)
        super.onStop()
    }

    override fun onPause() {
        super.onPause()
        savePrefs()
    }

    override fun onDestroy() {
        ui.removeCallbacks(ticker)
        // 发现监听必须停：`NsdManager` 的监听会一直占着（耗电），且下次启动重复注册会抛
        // `IllegalArgumentException`
        discovery?.stop()
        discovery = null
        teardown()
        super.onDestroy()
    }

    // ---------------------------------------------------------------- 地址归一

    /**
     * 把用户填的东西归一成 `host[:port]`。
     *
     * 电脑端「手机」面板显示的是**网页地址** `http://192.168.1.5:59123/`
     * （二维码扫出来就是它，页面上还有「复制」按钮），不是 `ws://`。
     * 所以这里三种写法都要认：
     *   - `192.168.1.5:59123`   —— 手敲 / 老习惯
     *   - `192.168.1.5`         —— 端口用默认
     *   - `http://192.168.1.5:59123/`、`ws://…`、`https://…` —— 从电脑端复制来的
     * 空的一律返回空串，由调用方报「请先填写」。
     */
    private fun normalizeTarget(raw: String): String {
        var s = raw.trim()
        if (s.isEmpty()) return ""
        // 去协议头（大小写都认）
        s = s.replace(Regex("(?i)^(https?|wss?)://"), "")
        // 去路径 / 查询 / 片段
        s = s.substringBefore('/').substringBefore('?').substringBefore('#')
        // 结尾的冒号（`192.168.1.5:`）是常见的半截输入，去掉再用默认端口
        if (s.endsWith(":")) s = s.dropLast(1)
        // IPv6 字面量必须带方括号：`[fe80::1]:59123`
        if (s.startsWith("[")) {
            val close = s.indexOf(']')
            if (close < 0) return ""
            val hostPart = s.substring(0, close + 1)
            val rest = s.substring(close + 1)
            return if (rest.startsWith(":")) hostPart + rest else hostPart
        }
        // 多个冒号又没方括号 = 裸 IPv6，不补端口
        if (s.count { it == ':' } > 1) return s
        return s
    }

    /**
     * 把 `host[:port]` 拆成 (host, port)，端口缺失或非法时用默认。
     *
     * **必须拆，不能把整个 target 当 host 传下去**：`Proto.wsUrl` 是 `"wss://$host:$port"`，
     * 直接传 `192.168.1.5:59200` 会拼出 `wss://192.168.1.5:59200:59123/ws` —— 不是合法 URL。
     * 而带端口的 target 恰恰是最常见的来源：电脑端二维码扫出来就是 `https://ip:端口/`，
     * mDNS 报上来的端口也是用户在电脑上自定义过的。
     *
     * 裸 IPv6（多个冒号、无方括号）整串当 host —— 它的地址里本来就有冒号。
     */
    private fun splitTarget(target: String): Pair<String, Int> {
        if (target.startsWith("[")) {
            val close = target.indexOf(']')
            if (close > 0) {
                val h = target.substring(0, close + 1)
                val rest = target.substring(close + 1)
                val p = if (rest.startsWith(":")) rest.drop(1).toIntOrNull() else null
                return h to (p?.takeIf { it in 1..65535 } ?: Proto.PORT)
            }
        }
        if (target.count { it == ':' } == 1) {
            val i = target.lastIndexOf(':')
            val p = target.substring(i + 1).toIntOrNull()
            if (p != null && p in 1..65535) return target.substring(0, i) to p
        }
        return target to Proto.PORT
    }

    // ---------------------------------------------------------------- 发现

    /**
     * 开始（或重新开始）自动发现。
     *
     * 「重新发现」按钮走这里：先 `stop()` 再 `start()`。**不复用旧监听** —— `NsdManager`
     * 的监听一旦注册就绑定了当时的网络状态，换了 Wi-Fi 之后它不会自己跟上，必须重来。
     */
    private fun rediscover() {
        startDiscovery()
        connState = getString(R.string.msg_discovering)
        updateStatus()
        log(getString(R.string.msg_discovering))
    }

    private fun startDiscovery() {
        discovery?.stop()
        found = false
        foundProto = null
        val d = Discovery(
            this,
            onFound = { h, p, name, proto ->
                runOnUiThread {
                    found = true
                    foundProto = proto
                    // 协议版本不符**不要连**：连上去会被服务端直接断开，只会让用户看到
                    // 一个没有解释的「已断开」。明确说是版本问题才有意义。
                    if (proto != null && proto != Proto.VERSION) {
                        connState = getString(R.string.msg_proto_mismatch, Proto.VERSION, proto)
                        updateStatus()
                        return@runOnUiThread
                    }
                    // **端口必须一起带上**：电脑端的端口是可编辑的（DESIGN.md §4.1.1），
                    // mDNS 报的是它在用的那个真实端口。只填 IP 会退回默认 59123，于是用户
                    // 改过端口之后手机这边「怎么都连不上」，而症状完全看不出是端口问题。
                    val target = if (p > 0) "$h:$p" else h
                    etHost.setText(target)
                    etHost.isEnabled = false        // 发现到了就不用手填
                    connState = getString(R.string.msg_found, name, h, p)
                    updateStatus()
                    // 自动连上：发现即连接，用户什么都不用点
                    if (ws == null && !connecting) connect()
                }
            },
            onLost = { name ->
                runOnUiThread {
                    found = false
                    etHost.isEnabled = true          // 服务没了 → 把手动填的路子还回来
                    if (ws == null) connState = getString(R.string.msg_lost, name)
                    updateStatus()
                }
            },
            onError = { msg ->
                runOnUiThread {
                    etHost.isEnabled = true
                    if (ws == null) connState = msg
                    updateStatus()
                }
            },
        )
        discovery = d
        if (!d.available) {
            etHost.isEnabled = true
            return
        }
        d.start()
    }

    /** 只重连 WebSocket，不重新发现（服务还在时用这个，快得多）。 */
    private fun reconnect() {
        val target = normalizeTarget(etHost.text.toString())
        if (target.isEmpty()) {
            showErr(getString(R.string.msg_no_target))
            return
        }
        // 发现结果已经失效（例如换了 Wi-Fi）时，重连多半会失败，直接退回重新发现
        if (!found) {
            rediscover()
            return
        }
        ws?.disconnect()
        ws = null
        connect()
    }

    // ---------------------------------------------------------------- 连接

    private fun connect() {
        val target = normalizeTarget(etHost.text.toString())
        if (target.isEmpty()) {
            showErr(getString(R.string.msg_no_target))
            return
        }
        // 端口单独存：mDNS 报上来的、以及用户从二维码复制来的地址都带自定义端口，
        // 只存 host 的话下次启动就退回默认端口，用户会看到「突然连不上了」
        val (h, p) = splitTarget(target)
        host = h
        port = p
        savePrefs()
        connecting = true
        ready = false
        rttMs = Double.NaN
        lastErr = ""
        connState = "连接中…"
        val client = WsClient(h, p, wsListener)
        ws = client
        client.connect()
        updateButtons()
        updateStatus()
        log("连接 ${Proto.wsUrl(host, port)}")
    }

    private fun disconnect() {
        val c = ws
        ws = null
        connecting = false
        c?.disconnect()
        onSessionEnded("已手动断开")
    }

    /** 断开 / 失败 / 电脑主动关闭后的统一收尾：停三条链路、清 ready。 */
    private fun onSessionEnded(reason: String) {
        ws = null
        connecting = false
        ready = false
        rttMs = Double.NaN
        connState = "已断开"
        applyState()
        updateButtons()
        updateStatus()
        log(reason)
    }

    private val wsListener = object : WsClient.Listener {
        override fun onOpen() {
            ui.post {
                connState = "已连接（等待 ready）"
                updateButtons()
                updateStatus()
                log("WebSocket 已连接，已发送 hello(proto=${Proto.VERSION})")
            }
        }

        override fun onMessage(msg: ServerMsg) {
            ui.post {
                when (msg) {
                    is ServerMsg.Ready -> onReady(msg)
                    is ServerMsg.Err -> showErr("电脑：${msg.msg}")
                    is ServerMsg.Pong -> Unit
                    is ServerMsg.Other -> log("未识别的消息 t=${msg.type}")
                }
                updateStatus()
            }
        }

        override fun onAudioPacket(packet: ByteArray) {
            val d = downlink
            if (d != null && speakerOn) d.enqueue(packet)
        }

        override fun onPong(id: Long, rtt: Double) {
            ui.post { rttMs = rtt }
        }

        override fun onClosed(reason: String) {
            ui.post { onSessionEnded("电脑关闭了连接：$reason") }
        }

        override fun onFailure(reason: String) {
            ui.post { onSessionEnded("连接失败：$reason") }
        }
    }

    private fun onReady(msg: ServerMsg.Ready) {
        ready = true
        connecting = false
        connState = "就绪"
        if (msg.proto != Proto.VERSION) {
            showErr("协议版本不符：电脑 ${msg.proto}，手机 ${Proto.VERSION}")
        }
        if (msg.codec.isNotEmpty() && msg.codec != Proto.CODEC) {
            showErr("电脑音频编码是 ${msg.codec}，手机只支持 ${Proto.CODEC}")
        }
        if (msg.rate > 0 && msg.rate != Proto.SAMPLE_RATE) {
            showErr("电脑音频是 ${msg.rate} Hz，手机播放按 ${Proto.SAMPLE_RATE} Hz 打开")
        }
        log(
            "ready · proto=${msg.proto} codec=${msg.codec} rate=${msg.rate} " +
                "ch=${msg.channels} frame=${msg.frameMs}ms 电脑侧客户端=${msg.clients}",
        )
        // sub 只由 applyStateInner 发一次（电脑端每收一条 sub 就注册一个新的出站订阅，
        // 重复发会泄漏订阅），这里不重复发。
        applyState()
        updateButtons()
    }

    private fun runHealthCheck() {
        val target = normalizeTarget(etHost.text.toString())
        if (target.isNotEmpty()) host = target
        healthText = "自检中…"
        updateStatus()
        HealthProbe.check(host, port) { ok, text ->
            ui.post {
                healthText = if (ok) text else "不可用 · $text"
                log("GET /health：$healthText")
                updateStatus()
            }
        }
    }

    // ---------------------------------------------------------------- 三条链路的编排

    /**
     * 唯一的「期望 vs 实际」对账入口。所有开关变化 / 连接状态变化 / 权限变化都走这里，
     * 不在别处启停链路（一个功能只有一条实现路径）。
     *
     * 带重入保护：开关回调会再次调用本方法（改 `isChecked` 触发监听器）。
     */
    private fun applyState() {
        if (applyingState) return
        applyingState = true
        try {
            applyStateInner()
        } finally {
            applyingState = false
        }
    }

    private fun applyStateInner() {
        val wantMic = swMic.isChecked && ready && hasMicPermission()
        if (wantMic && uplink == null) {
            if (!startUplink()) {
                swMic.isChecked = false
                updateButtons()
            }
        } else if (!wantMic && uplink != null) {
            uplink?.stop()
            uplink = null
            CaptureService.stop(this)
            log("麦克风上行已停止")
            updateButtons()
        }

        speakerOn = swSpeaker.isChecked && ready
        if (speakerOn && downlink == null) {
            val d = SpeakerDownlink()
            if (d.start()) {
                downlink = d
                ws?.sendText(Proto.sub())
                log("电脑音频播放已开启（解码 ${d.decoderName}）")
            } else {
                speakerOn = false
                swSpeaker.isChecked = false
                showErr(d.lastError ?: "下行播放启动失败")
            }
        } else if (!speakerOn && downlink != null) {
            downlink?.stop()
            downlink = null
            // 协议没有退订消息，电脑会继续推包；这里只是不再解码/播放
            log("电脑音频播放已停止（电脑端仍在推流，协议无退订消息，重连才会停止）")
        }

        remoteTextOn = swRemote.isChecked && swRemoteText.isChecked && ready
        remoteKeysOn = swRemote.isChecked && swRemoteKeys.isChecked && ready
        updateButtons()
    }

    /** 起上行：先起前台服务（Android 9+ 后台麦克风的前提），再开 AudioRecord。 */
    private fun startUplink(): Boolean {
        CaptureService.start(this, getString(R.string.notif_starting))
        val u = MicUplink(
            context = this,
            onPacket = { pkt -> ws?.sendAudio(pkt, pkt.size) },
            onError = { reason -> ui.post { showErr(reason) } },
        )
        if (!u.start()) {
            CaptureService.stop(this)
            return false
        }
        uplink = u
        log(
            "麦克风上行已启动：采集 ${OpusSupport.pcmName(u.recordEncoding)} @${MicUplink.SAMPLE_RATE}Hz " +
                "→ 编码 ${u.encoderName}",
        )
        return true
    }

    private fun teardown() {
        uplink?.stop()
        uplink = null
        downlink?.stop()
        downlink = null
        CaptureService.stop(this)
        ws?.disconnect()
        ws = null
        ready = false
        connecting = false
    }

    // ---------------------------------------------------------------- 远程输入

    /**
     * 实体键转发：只转发**硬件**事件（`deviceId >= 0`；输入法合成的键 `deviceId == -1`，
     * 那条路走 `text`，避免同一次按键既发 key 又发 text）。系统键（返回/主屏/音量…）不转发，
     * 否则手机自己就用不了了。转发成功即**吞掉**事件，交给电脑去处理。
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (remoteKeysOn && ready) {
            val code = event.keyCode
            if (code != 0 && code !in SYSTEM_KEYS && event.deviceId >= 0) {
                sendKey(code, event.action == KeyEvent.ACTION_DOWN)
                return true
            }
        }
        return super.dispatchKeyEvent(event)
    }

    private fun sendKey(code: Int, down: Boolean) {
        ws?.sendText(Proto.key(code, down))
    }

    private fun sendText(s: String) {
        if (!remoteTextOn || !ready) return
        ws?.sendText(Proto.text(s))
    }

    private fun refreshRemoteSwitches() {
        swRemoteText.isEnabled = swRemote.isChecked
        swRemoteKeys.isEnabled = swRemote.isChecked
    }

    // ---------------------------------------------------------------- 权限

    private fun hasMicPermission(): Boolean =
        ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED

    private fun requestPermissions() {
        val want = ArrayList<String>()
        if (!hasMicPermission()) want.add(Manifest.permission.RECORD_AUDIO)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val p = Manifest.permission.POST_NOTIFICATIONS
            if (ContextCompat.checkSelfPermission(this, p) != PackageManager.PERMISSION_GRANTED) {
                want.add(p)
            }
        }
        if (want.isNotEmpty()) {
            permissionLauncher.launch(want.toTypedArray())
        }
    }

    // ---------------------------------------------------------------- 状态显示

    private fun updateButtons() {
        val online = ready
        btnConnect.text = if (ws != null || connecting) getString(R.string.btn_disconnect) else getString(R.string.btn_connect)
        btnConnect.isEnabled = !connecting
        swMic.isEnabled = online
        swSpeaker.isEnabled = online
        swRemote.isEnabled = online
        swRemoteText.isEnabled = online && swRemote.isChecked
        swRemoteKeys.isEnabled = online && swRemote.isChecked
        etText.hint = if (remoteTextOn) {
            getString(R.string.hint_text_synced)
        } else {
            getString(R.string.hint_text_local)
        }
    }

    /** 固定 7 行读数；行数固定，数值变长也不跳布局。 */
    private fun updateStatus() {
        val c = ws
        val enc = uplink
        val dec = downlink
        val line = StringBuilder()
        line.append(row(getString(R.string.status_conn), connState))
        line.append(row(getString(R.string.status_host), Proto.wsUrl(host, port)))
        line.append(
            row(
                getString(R.string.status_rtt),
                if (rttMs.isNaN()) "测量中…" else String.format("%.1f ms", rttMs),
            ),
        )
        line.append(
            row(
                getString(R.string.status_up),
                if (c == null) {
                    "未连接"
                } else {
                    "${c.txPackets.get()} 包" + if (enc != null) {
                        " · 编码 ${enc.encoderName} · 采集 ${OpusSupport.pcmName(enc.recordEncoding)}" +
                            " · 电平 ${String.format("%.2f", enc.level)}"
                    } else {
                        " · 未开启"
                    }
                },
            ),
        )
        line.append(
            row(
                getString(R.string.status_down),
                if (c == null) {
                    "未连接"
                } else {
                    "${c.rxPackets.get()} 包" + if (dec != null) {
                        " · 解码 ${dec.decoderName} · 水位 ${dec.bufferedMs().toInt()} ms"
                    } else {
                        " · 未订阅"
                    }
                },
            ),
        )
        line.append(
            row(
                getString(R.string.status_play),
                if (dec == null) {
                    "未开启"
                } else {
                    "欠载 ${dec.underruns} · 丢包 ${dec.dropped} · 峰值 ${String.format("%.2f", dec.peak)}" +
                        (dec.lastError?.let { " · $it" } ?: "")
                },
            ),
        )
        line.append(row(getString(R.string.status_opus), opusDetail))
        tvStatus.text = line.toString()

        tvHealth.text = getString(R.string.status_health) + healthText
        tvErr.text = lastErr
        tvErr.visibility = if (lastErr.isEmpty()) android.view.View.GONE else android.view.View.VISIBLE
    }

    private fun row(label: String, value: String): String =
        String.format("%-11s %s%n", label, value)

    private fun showErr(msg: String) {
        lastErr = msg
        Log.w(TAG, msg)
        log("! $msg")
        updateStatus()
    }

    private fun log(msg: String) {
        ui.post {
            val t = java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.getDefault())
                .format(java.util.Date())
            tvLog.append("[$t] $msg\n")
            val lines = tvLog.text.toString().split("\n")
            if (lines.size > MAX_LOG_LINES) {
                tvLog.text = lines.subList(lines.size - MAX_LOG_LINES, lines.size).joinToString("\n")
            }
            tvLog.post { tvLog.scrollTo(0, tvLog.height) }
        }
    }

    // ---------------------------------------------------------------- 偏好 & 绑定

    private fun bindViews() {
        etHost = findViewById(R.id.etHost)
        btnConnect = findViewById(R.id.btnConnect)
        btnHealth = findViewById(R.id.btnHealth)
        btnRediscover = findViewById(R.id.btnRediscover)
        btnReconnect = findViewById(R.id.btnReconnect)
        tvStatus = findViewById(R.id.tvStatus)
        tvHealth = findViewById(R.id.tvHealth)
        tvErr = findViewById(R.id.tvErr)
        swMic = findViewById(R.id.swMic)
        swSpeaker = findViewById(R.id.swSpeaker)
        swRemote = findViewById(R.id.swRemote)
        swRemoteText = findViewById(R.id.swRemoteText)
        swRemoteKeys = findViewById(R.id.swRemoteKeys)
        etText = findViewById(R.id.etText)
        tvLog = findViewById(R.id.tvLog)
    }

    private fun restorePrefs() {
        val p = getSharedPreferences(PREFS, MODE_PRIVATE)
        host = p.getString("host", DEFAULT_HOST) ?: DEFAULT_HOST
        port = p.getInt("port", Proto.PORT)
        // 回填成带端口的完整形式，界面上一眼能看到连的是哪个端口
        etHost.setText(if (port == Proto.PORT) host else "$host:$port")
        swMic.isChecked = p.getBoolean("mic", false)
        swSpeaker.isChecked = p.getBoolean("speaker", false)
        swRemote.isChecked = p.getBoolean("remote", false)
        swRemoteText.isChecked = p.getBoolean("remoteText", true)
        swRemoteKeys.isChecked = p.getBoolean("remoteKeys", true)
    }

    private fun savePrefs() {
        // 存归一后的 host + 拆出来的端口（别把 `http://…/` 整串存进去）
        val typed = normalizeTarget(etHost.text.toString())
        if (typed.isNotEmpty()) {
            val (h, pt) = splitTarget(typed)
            host = h
            port = pt
        }
        getSharedPreferences(PREFS, MODE_PRIVATE).edit()
            .putString("host", host)
            .putInt("port", port)
            .putBoolean("mic", swMic.isChecked)
            .putBoolean("speaker", swSpeaker.isChecked)
            .putBoolean("remote", swRemote.isChecked)
            .putBoolean("remoteText", swRemoteText.isChecked)
            .putBoolean("remoteKeys", swRemoteKeys.isChecked)
            .apply()
    }
}