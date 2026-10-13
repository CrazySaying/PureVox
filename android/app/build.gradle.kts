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

// :app —— 唯一的模块。**没有** externalNativeBuild：Opus 用的是预编译入库的
// `src/main/jniLibs/arm64-v8a/libpvopus.so`（libopus 1.5.2 + `cpp/pv_opus.cpp` 的 JNI
// 转发层），所以 Gradle 构建**不需要 NDK / CMake**，CI 也不用装。重建那个 .so 用
// `tools/automation/build_opus_android.ps1`。
//
// 为什么不用系统 MediaCodec：Android 上从来没有 `audio/opus` 这个 mime，系统 Opus codec
// 注册的是 `audio/raw` / `audio/ogg;codecs=opus` / `audio/webm;codecs=opus`，后两者只认
// 容器封装，喂裸 Opus 包会输出垃圾 PCM（听感满屏刺啦且不报错）。
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.purevox.mic"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.purevox.mic"
        minSdk = 24
        targetSdk = 34
        // versionCode 必须**单调递增**才能让应用内更新检查判出新旧，故取 `yyMMddHHmm`
        // （如 2610081430，int32 内）。由 CI 传入（`-PpvVersionCode=`，见
        // tools/automation/version.sh 的 ACODE）；本机默认 1。
        versionCode = ((findProperty("pvVersionCode") as String?)?.toIntOrNull() ?: 1)
        // 版本名 = 年.月.日，由 CI 传入（`-PpvVersion=`，见 tools/automation/version.sh）；本机默认 1.0。
        versionName = (findProperty("pvVersion") as String?) ?: "1.0"
    }

    buildTypes {
        release {
            // 反射/无：全是系统 API + OkHttp，混淆收益低，保持可读可审计
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
        debug {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        buildConfig = false
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    // 明文 ws:// —— 没有 TLS 依赖，也没有证书信任绕过（新版协议就是明文 + usesCleartextTraffic）
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
}