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

// PureVox 的 Opus JNI 转发层 —— 链接 libopus 1.5.2（BSD-3-Clause，见 LICENSE-THIRD-PARTY.txt）。
//
// ## 为什么不用系统 MediaCodec
//
// Android 上**从来没有 `audio/opus` 这个 mime**。系统里的 Opus codec 注册的是
// `audio/raw` / `audio/ogg;codecs=opus` / `audio/webm;codecs=opus`，后两者只认**容器封装**的
// 输入输出：把裸 Opus 包喂进去，解码器会输出垃圾 PCM（听起来全是刺啦）。而为了吃裸包就
// 要自己实现 Ogg / WebM 封装，那是拿「省一个依赖」换「自己维护一个协议」——libopus 才是
// Opus 的参考实现（Chrome / WebRTC / Firefox 同款），而且正好和网页版的 libopus-wasm
// 是同一个编码器，两端行为天然对齐。
//
// ## 这一层只做转发
//
// 帧长、码率、PCM 约定全由 Kotlin 侧决定，这里不掺任何策略，方便 Kotlin 那边的逻辑
// 一眼读懂、不用钻进 C 里看。
//
// ## float ↔ int16 为什么在这里转
//
// libopus 的标准输入是 `opus_int16`（`OPUS_SET_LSB_DEPTH(16)`；`OPUS_SET_INPUT_SIGNATURE`
// 那套只在**自定义构建**里有，标准构建没有）。而 Kotlin 侧全链路都是 `FloatArray`
// （`AudioRecord` 给 float、`AudioTrack` 吃 float），改成 ShortArray 要动四个文件、且麦克风
// 侧本来也得先转成 int16 才能喂 AudioTrack —— 转换一次都省不掉。
//
// 所以**在这里转**：Java 侧零感知，转换只发生一次，量化到 16 bit 对语音（Opus 本身就是
// 16 bit 精度）不构成可听的损失。

#include <jni.h>
#include <opus.h>

#include <cmath>

namespace {

// libopus 的对象**不是线程安全的**，句柄由 Kotlin 侧按用途分开持有（上行一个、下行一个），
// 各自只在自己那条线程上用。
struct Enc {
  OpusEncoder *enc;
};

struct Dec {
  OpusDecoder *dec;
};

// 线程局部存最近一次 opus 错误码。JNI 返回值空间紧张（还要返回包长），错误细节走这里，
// Kotlin 侧用 lastError() / lastErrorCode() 取。
thread_local int g_lastError = OPUS_OK;

// 帧长上限：libopus 允许 2.5 ~ 120 ms @48 kHz，取上限做栈缓冲。
constexpr int kMaxFrameSamples = 5760;

// float(-1..1) → int16。带**削顶**和半 LSB 舍入：`x * 32768` 后四舍五入，
// 超出范围的直接夹到 ±32767/-32768（int16 溢出是回绕，会变成反向的爆音，比削顶糟得多）。
inline void FloatToInt16(const jfloat *in, int n, opus_int16 *out) {
  for (int i = 0; i < n; ++i) {
    float s = in[i];
    if (!(s > -1.0f)) s = -1.0f;   // 同时挡住 NaN
    if (s > 1.0f) s = 1.0f;
    float q = s * 32768.0f;
    int32_t v = static_cast<int32_t>(q >= 0.0f ? q + 0.5f : q - 0.5f);
    if (v > 32767) v = 32767;
    if (v < -32768) v = -32768;
    out[i] = static_cast<opus_int16>(v);
  }
}

inline void Int16ToFloat(const opus_int16 *in, int n, jfloat *out) {
  for (int i = 0; i < n; ++i) out[i] = static_cast<float>(in[i]) * (1.0f / 32768.0f);
}

}  // namespace

extern "C" {

// ---------------------------------------------------------------- 编码（手机麦克风 → 电脑）

JNIEXPORT jlong JNICALL Java_com_purevox_mic_audio_OpusNative_encCreate(
    JNIEnv *, jclass, jint sample_rate, jint channels, jint bitrate, jint application) {
  int err = OPUS_OK;
  OpusEncoder *enc = opus_encoder_create(static_cast<opus_int32>(sample_rate), channels,
                                         application, &err);
  if (enc == nullptr || err != OPUS_OK) {
    g_lastError = (err != OPUS_OK) ? err : OPUS_ALLOC_FAIL;
    return 0;
  }
  opus_encoder_ctl(enc, OPUS_SET_LSB_DEPTH(16));
  opus_encoder_ctl(enc, OPUS_SET_BITRATE(bitrate));
  // 语音场景：把比特花在语音的频谱形状上，比通用模式省。
  opus_encoder_ctl(enc, OPUS_SET_SIGNAL(OPUS_SIGNAL_VOICE));
  opus_encoder_ctl(enc, OPUS_SET_COMPLEXITY(5));
  opus_encoder_ctl(enc, OPUS_SET_VBR(1));
  // 带内 FEC + 丢包率预估：网络抖一下时解码器能救回一点，比直接丢整帧好。
  opus_encoder_ctl(enc, OPUS_SET_INBAND_FEC(1));
  opus_encoder_ctl(enc, OPUS_SET_PACKET_LOSS_PERC(5));

  g_lastError = OPUS_OK;
  return reinterpret_cast<jlong>(new Enc{enc});
}

/**
 * 编码一帧。[samples] 必须是合法帧长（2.5 / 5 / 10 / 20 / 40 / 60 ms @48 kHz），
 * 非法长度 libopus 会返回 `OPUS_BAD_ARG`。返回包字节数，< 0 是错误码。
 */
JNIEXPORT jint JNICALL Java_com_purevox_mic_audio_OpusNative_encEncode(
    JNIEnv *env, jclass, jlong handle, jfloatArray pcm, jint samples, jbyteArray out) {
  Enc *w = reinterpret_cast<Enc *>(handle);
  if (w == nullptr || w->enc == nullptr || pcm == nullptr || out == nullptr) {
    g_lastError = OPUS_BAD_ARG;
    return OPUS_BAD_ARG;
  }
  const int n = samples;
  if (n <= 0 || n > kMaxFrameSamples) {
    g_lastError = OPUS_BAD_ARG;
    return OPUS_BAD_ARG;
  }
  const jsize cap = env->GetArrayLength(out);

  // 栈上转 int16：kMaxFrameSamples × 2 B = 11.5 KB，栈完全放得下，不碰堆。
  opus_int16 tmp[kMaxFrameSamples];
  jfloat *p = env->GetFloatArrayElements(pcm, nullptr);
  FloatToInt16(p, n, tmp);
  env->ReleaseFloatArrayElements(pcm, p, JNI_ABORT);

  jbyte *o = env->GetByteArrayElements(out, nullptr);
  const int r = opus_encode(w->enc, tmp, n, reinterpret_cast<unsigned char *>(o), cap);
  env->ReleaseByteArrayElements(out, o, 0);

  g_lastError = (r < 0) ? r : OPUS_OK;
  return r;
}

/** 编码器前瞻（样本数）：调用方要据此推迟输出才能与解码端对齐。 */
JNIEXPORT jint JNICALL Java_com_purevox_mic_audio_OpusNative_encLookahead(JNIEnv *, jclass,
                                                                         jlong handle) {
  Enc *w = reinterpret_cast<Enc *>(handle);
  if (w == nullptr || w->enc == nullptr) return 0;
  opus_int32 v = 0;
  opus_encoder_ctl(w->enc, OPUS_GET_LOOKAHEAD(&v));
  return v;
}

JNIEXPORT void JNICALL Java_com_purevox_mic_audio_OpusNative_encDestroy(JNIEnv *, jclass,
                                                                        jlong handle) {
  Enc *w = reinterpret_cast<Enc *>(handle);
  if (w == nullptr) return;
  if (w->enc != nullptr) opus_encoder_destroy(w->enc);
  delete w;
}

// ---------------------------------------------------------------- 解码（电脑音频 → 手机）

JNIEXPORT jlong JNICALL Java_com_purevox_mic_audio_OpusNative_decCreate(
    JNIEnv *, jclass, jint sample_rate, jint channels) {
  int err = OPUS_OK;
  OpusDecoder *dec = opus_decoder_create(static_cast<opus_int32>(sample_rate), channels, &err);
  if (dec == nullptr || err != OPUS_OK) {
    g_lastError = (err != OPUS_OK) ? err : OPUS_ALLOC_FAIL;
    return 0;
  }
  g_lastError = OPUS_OK;
  return reinterpret_cast<jlong>(new Dec{dec});
}

/**
 * 解一个包。[max_samples] 是输出缓冲能装的样本数。
 *
 * [pkt] 传 null 时走 **PLC（丢包隐藏）**：不解码，按上一帧的频谱做补偿。网络抖动时用它顶一帧，
 * 比输出静音自然。返回解出的样本数，< 0 是错误码。
 */
JNIEXPORT jint JNICALL Java_com_purevox_mic_audio_OpusNative_decDecode(
    JNIEnv *env, jclass, jlong handle, jbyteArray pkt, jint len, jfloatArray out,
    jint max_samples) {
  Dec *w = reinterpret_cast<Dec *>(handle);
  if (w == nullptr || w->dec == nullptr || out == nullptr) {
    g_lastError = OPUS_BAD_ARG;
    return OPUS_BAD_ARG;
  }
  jsize cap = env->GetArrayLength(out);
  jint n = cap < max_samples ? cap : max_samples;
  if (n > kMaxFrameSamples) n = kMaxFrameSamples;

  const unsigned char *p = nullptr;
  jsize pl = 0;
  if (pkt != nullptr) {
    jbyte *raw = env->GetByteArrayElements(pkt, nullptr);
    pl = env->GetArrayLength(pkt);
    if (len >= 0 && len < pl) pl = static_cast<jsize>(len);
    p = reinterpret_cast<const unsigned char *>(raw);
    // raw 不能在 opus_decode 之前释放——它就是输入缓冲区。
    // 统一在下面按 p != nullptr 的同一个分支释放，避免出现「early return 忘了放」。
  }

  opus_int16 tmp[kMaxFrameSamples];
  const int r = opus_decode(w->dec, p, pl, tmp, n, p == nullptr ? 1 : 0);
  if (p != nullptr) {
    env->ReleaseByteArrayElements(
        pkt, reinterpret_cast<jbyte *>(const_cast<unsigned char *>(p)), JNI_ABORT);
  }
  if (r < 0) {
    g_lastError = r;
    return r;
  }

  jfloat *o = env->GetFloatArrayElements(out, nullptr);
  Int16ToFloat(tmp, r, o);
  env->ReleaseFloatArrayElements(out, o, 0);
  g_lastError = OPUS_OK;
  return r;
}

JNIEXPORT void JNICALL Java_com_purevox_mic_audio_OpusNative_decDestroy(JNIEnv *, jclass,
                                                                        jlong handle) {
  Dec *w = reinterpret_cast<Dec *>(handle);
  if (w == nullptr) return;
  if (w->dec != nullptr) opus_decoder_destroy(w->dec);
  delete w;
}

// ---------------------------------------------------------------- 元信息

JNIEXPORT jstring JNICALL Java_com_purevox_mic_audio_OpusNative_version(JNIEnv *env, jclass) {
  return env->NewStringUTF(opus_get_version_string());
}

/** 最近一次 opus 错误的英文说明（`opus_strerror`）。 */
JNIEXPORT jstring JNICALL Java_com_purevox_mic_audio_OpusNative_lastError(JNIEnv *env, jclass) {
  return env->NewStringUTF(opus_strerror(g_lastError));
}

JNIEXPORT jint JNICALL Java_com_purevox_mic_audio_OpusNative_lastErrorCode(JNIEnv *, jclass) {
  return g_lastError;
}

}  // extern "C"