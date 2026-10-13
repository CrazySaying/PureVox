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

package com.purevox.mic.audio

import android.util.Log

/**
 * libopus 的 JNI 绑定（`libpvopus.so` + `cpp/pv_opus.cpp`，链 libopus 1.5.2）。
 *
 * ## 为什么不用系统 MediaCodec
 *
 * Android 上从来没有 `audio/opus` 这个 mime。系统里的 Opus codec 注册的是
 * `audio/raw` / `audio/ogg;codecs=opus` / `audio/webm;codecs=opus`，后两者只认**容器封装**的
 * 输入输出——把裸 Opus 包喂进去，解码器输出的是垃圾 PCM（听起来全是刺啦）。要吃裸包就得
 * 自己实现 Ogg / WebM 封装，那是拿「省一个依赖」换「自己维护一个协议」。
 *
 * libopus 是 Opus 的参考实现（Chrome / WebRTC / Firefox 同款），跨设备行为一致，而且正好
 * 和网页版的 libopus-wasm 是同一个编码器，两端天然对齐。
 *
 * ## 加载失败要说清楚
 *
 * 库是预编译入库的（`jniLibs/arm64-v8a/libpvopus.so`），加载失败只有一种现实原因：**设备不是
 * arm64-v8a**（x86 模拟器 / 其它 ABI）。这时不能静默返回「不可用」，得让人知道该装什么包。
 */
object OpusNative {

    private const val TAG = "PureVoxOpus"

    /** 加载失败的原因；null 表示加载成功。[available] 才是判据。 */
    @Volatile
    var loadError: String? = null
        private set

    @Volatile
    var version: String = "未加载"
        private set

    val available: Boolean get() = loadError == null

    init {
        try {
            System.loadLibrary("pvopus")
            version = version()
            Log.i(TAG, "libopus 就绪：$version")
        } catch (e: Throwable) {
            // 只在「首次触碰」时才报错一次，且带原因：多半是 ABI 不匹配
            loadError = "libpvopus.so 加载失败（${e.message ?: e.javaClass.simpleName}）。" +
                "本包只含 arm64-v8a；x86 模拟器或非常规 ABI 设备用不了。"
            Log.e(TAG, "libopus 加载失败", e)
        }
    }

    /** 加载失败时抛出统一异常，消息已经带原因，由界面直接显示。 */
    fun require(): OpusNative = this.also {
        if (!available) throw IllegalStateException(loadError!!)
    }

    // ------------------------------------------------------------ 编码（手机麦克风 → 电脑）

    /**
     * 建编码器。返回 0 表示失败（看 [lastErrorCode]）。
     *
     * @param application `OPUS_APPLICATION_VOIP(2048)` / `AUDIO(2049)` / `RESTRICTED_LOWDELAY(2051)`
     */
    @JvmStatic external fun encCreate(
        sampleRate: Int,
        channels: Int,
        bitrate: Int,
        application: Int,
    ): Long

    /** 编码一帧；[samples] 必须是合法帧长（@48kHz 是 120/240/480/960/1920/2880）。返回包字节数，<0 是错误码。 */
    @JvmStatic external fun encEncode(
        handle: Long,
        pcm: FloatArray,
        samples: Int,
        out: ByteArray,
    ): Int

    /** 编码器前瞻（样本数）：要推迟输出才能与解码端对齐。 */
    @JvmStatic external fun encLookahead(handle: Long): Int

    @JvmStatic external fun encDestroy(handle: Long)

    // ------------------------------------------------------------ 解码（电脑音频 → 手机）

    @JvmStatic external fun decCreate(sampleRate: Int, channels: Int): Long

    /**
     * 解一个包；[maxSamples] 是 [out] 能装的样本数。返回解出的样本数，<0 是错误码。
     *
     * [pkt] 传 null 走 **PLC 丢包隐藏**：不解码，按上一帧频谱补偿，比输出静音自然。
     */
    @JvmStatic external fun decDecode(
        handle: Long,
        pkt: ByteArray?,
        len: Int,
        out: FloatArray,
        maxSamples: Int,
    ): Int

    @JvmStatic external fun decDestroy(handle: Long)

    // ------------------------------------------------------------ 元信息

    /** libopus 版本串（`libopus 1.5.2` 之类）。 */
    @JvmStatic external fun version(): String

    /** 最近一次 opus 错误的英文说明。 */
    @JvmStatic external fun lastError(): String

    @JvmStatic external fun lastErrorCode(): Int

    /**
     * 与 JNI `application` 参数配套的编码模式。
     *
     * `VOIP` = 针对语音优化（比特花在语音的频谱形状上），正好是我们这个场景；
     * 另外两个是 `AUDIO(2049)` 通用音乐、`RESTRICTED_LOWDELAY(2051)` 低延迟免回声算法。
     */
    const val APP_VOIP = 2048
}