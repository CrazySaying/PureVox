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

package com.purevox.mic.update

import android.content.Context
import android.os.Build
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/**
 * 应用内更新：**唯一数据源 = 本仓库 GitHub Releases**（与电脑端 `src-tauri/src/update/`
 * 同一个仓库、同一套命名规则）。
 *
 * 判定依据是 **tag 的完整时间戳**，不是显示版本号：显示版本 `yy.M.d` 同日会重复，
 * 而 tag `v<yyyy.MM.dd.HHmm>` 按时间全序。本机版本则是安装包的 `versionCode`
 * （`yyMMddHHmm ÷ 10`，见 `tools/automation/version.py` 的 `ACODE`）—— 两者同源，直接比整数
 * 大小即可，且 **versionCode 单调递增**正是为此设计的（`build.gradle.kts` 注释）。
 * **÷10 必须两侧一致**（见 `stampToCode`）：versionCode 是 Int32，十位数值装不进去。
 *
 * 产物**按文件名后缀匹配**（`*.apk`），不看版本号（同 `update/install_*.rs` 的思路）：
 * `release.yml` 会统一重命名产物，后缀因此稳定。
 *
 * **开发构建不下结论**：本机 `versionCode = 1` 时（`-PpvVersionCode` 没传）没有可比的时间戳，
 * 报 [UpdateResult.DevBuild] 而不是「已是最新」。
 */
object UpdateChecker {

    /** 唯一数据源。与电脑端 `update::REPO` 一致。 */
    private const val REPO = "a2heng/PureVox"
    private const val API = "https://api.github.com/repos/$REPO/releases/latest"

    /**
     * 没有版本时间戳的构建（本机 `assembleRelease` 默认 versionCode=1）。
     *
     * 注意 **2026-10-17 及更早的 CI 包也叫 1**（当时的 `ACODE` 十位数溢出被静默丢成 1）：
     * 那些包会一直报「开发构建」，只能手动装下一版 —— 修好之后的包 versionCode 正常。
     */
    private const val DEV_VERSION_CODE = 1L

    /** versionCode 上限（Play 的硬限值，也确保 Int32 装得下；与 `version.py` 同值）。 */
    private const val MAX_VERSION_CODE = 2_100_000_000L

    private val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(20, TimeUnit.SECONDS)
        .build()

    /** 一个 release 里我们关心的字段。 */
    data class ReleaseInfo(
        val tag: String,
        val versionCode: Int,
        val displayVersion: String,
        val apkName: String,
        val apkUrl: String,
        val notes: String,
    )

    /** 检查结果。三种「成功」之间意义不同，别混成一团。 */
    sealed class UpdateResult {
        /** 有新版本，可下载。 */
        data class Available(val release: ReleaseInfo, val installedCode: Long) : UpdateResult()

        /** 已是最新。 */
        data class UpToDate(val installedCode: Long) : UpdateResult()

        /** 开发构建，没有时间戳可比。 */
        data class DevBuild(val installedCode: Long) : UpdateResult()

        /** 网络 / 解析失败（原文显示给用户，不静默）。 */
        data class Failed(val reason: String) : UpdateResult()
    }

    /** 本机安装包的 `versionCode`（长整型，避开历史 32 位截断）。 */
    fun installedCode(ctx: Context): Long {
        val pi = ctx.packageManager.getPackageInfo(ctx.packageName, 0)
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            pi.longVersionCode
        } else {
            @Suppress("DEPRECATION")
            pi.versionCode.toLong()
        }
    }

    /** 本机显示版本名（`26.10.8`）。 */
    fun installedName(ctx: Context): String {
        val pi = ctx.packageManager.getPackageInfo(ctx.packageName, 0)
        return pi.versionName ?: "?"
    }

    /**
     * `v2026.10.08.1430` → `261008143`（= `ACODE`）。格式不对 / 越界返回 `null`。
     *
     * **必须补零再拼**：先在**定长十位**上拼出 `yyMMddHHmm`（`26` + `10` + `08` + `1430`），
     * 再整除 10；若按整数去前导零拼会得到 `26108 1430` 这种位数不定的串，与安装包的值不可比。
     *
     * **÷10 不能省**：十位的 `2610172359`（2.6e9）超过 versionCode 的 Int32 上限
     * 2147483647 —— 溢出成 `null` 的表现是**整条链静默失效**：Gradle 侧退回 `versionCode=1`，
     * 应用侧认不出 release（报「没有可用的 APK 资产」），2026-10-17 实测踩到。÷10 后是
     * 10 分钟粒度、单调、上限 991231235，与 `version.py` 逐字一致。
     */
    fun stampToCode(tag: String): Int? {
        val t = tag.trim().removePrefix("v")
        val parts = t.split(".")
        if (parts.size != 4) return null
        val y = parts[0].toIntOrNull() ?: return null
        val mo = parts[1].toIntOrNull() ?: return null
        val d = parts[2].toIntOrNull() ?: return null
        val hm = parts[3].toIntOrNull() ?: return null
        if (mo !in 1..12 || d !in 1..31 || hm !in 0..2359) return null
        if (hm % 100 > 59) return null // 2359 合法，2360 不合法
        val yy = y % 100
        val code = "%02d%02d%02d%04d".format(yy, mo, d, hm).toLong() / 10L
        if (code !in 1..MAX_VERSION_CODE) return null
        return code.toInt()
    }

    /** `v2026.10.08.1430` → `26.10.8`（显示版本，去前导零，与 `version.py` 的 VERSION 同构）。 */
    fun stampToVersion(tag: String): String? {
        val t = tag.trim().removePrefix("v")
        val parts = t.split(".")
        if (parts.size != 4) return null
        val y = parts[0].toIntOrNull() ?: return null
        val mo = parts[1].toIntOrNull() ?: return null
        val d = parts[2].toIntOrNull() ?: return null
        return "${y % 100}.$mo.$d"
    }

    /**
     * 检查更新。**回调在 OkHttp 线程**，调用方负责切回主线程再动 UI。
     * 手动触发（界面按钮），不自动检查 —— 避免每次启动都打一次 GitHub API。
     */
    fun check(ctx: Context, onDone: (UpdateResult) -> Unit) {
        val installed = installedCode(ctx)
        if (installed <= DEV_VERSION_CODE) {
            onDone(UpdateResult.DevBuild(installed))
            return
        }
        val req = Request.Builder()
            .url(API)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "PureVox-Android")
            .get()
            .build()
        client.newCall(req).enqueue(object : okhttp3.Callback {
            override fun onFailure(call: okhttp3.Call, e: java.io.IOException) {
                onDone(UpdateResult.Failed(e.message ?: e.javaClass.simpleName))
            }

            override fun onResponse(call: okhttp3.Call, response: okhttp3.Response) {
                response.use { r ->
                    if (!r.isSuccessful) {
                        onDone(UpdateResult.Failed("HTTP ${r.code}"))
                        return
                    }
                    val body = r.body?.string()
                    if (body.isNullOrBlank()) {
                        onDone(UpdateResult.Failed("空响应"))
                        return
                    }
                    val info = try {
                        parseRelease(body)
                    } catch (e: Exception) {
                        onDone(UpdateResult.Failed("解析失败：${e.message ?: e.javaClass.simpleName}"))
                        return
                    }
                    if (info == null) {
                        onDone(UpdateResult.Failed("最新 release 没有可用的 APK 资产"))
                        return
                    }
                    onDone(
                        if (info.versionCode.toLong() > installed) {
                            UpdateResult.Available(info, installed)
                        } else {
                            UpdateResult.UpToDate(installed)
                        },
                    )
                }
            }
        })
    }

    /** 从 `/releases/latest` 的 JSON 里取 tag + 第一个 `.apk` 资产；取不到返回 `null`。 */
    private fun parseRelease(body: String): ReleaseInfo? {
        val o = JSONObject(body)
        val tag = o.optString("tag_name")
        val code = stampToCode(tag) ?: return null
        val ver = stampToVersion(tag) ?: return null
        val assets = o.optJSONArray("assets") ?: return null
        var apkName = ""
        var apkUrl = ""
        for (i in 0 until assets.length()) {
            val a = assets.optJSONObject(i) ?: continue
            val name = a.optString("name")
            if (name.endsWith(".apk", ignoreCase = true)) {
                apkName = name
                apkUrl = a.optString("browser_download_url")
                break
            }
        }
        if (apkName.isEmpty() || apkUrl.isEmpty()) return null
        return ReleaseInfo(
            tag = tag,
            versionCode = code,
            displayVersion = ver,
            apkName = apkName,
            apkUrl = apkUrl,
            notes = o.optString("body"),
        )
    }
}
