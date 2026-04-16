"use client";

import JSON5 from "json5";
import { useState, useEffect, useRef, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import { Path } from "../constant";
import Locale, { getLang } from "../locales";
import {
  getHealth,
  getConfig,
  saveConfig,
  reloadConfig,
  wechatQrStart,
  wechatQrStatus,
  testProviderKey,
  listProviderModels,
  setGatewayUrl,
  setAuthToken,
} from "../lib/rsclaw-api";
import { markSetupComplete } from "../lib/first-launch";
import { toast } from "../lib/toast";
import {
  type ApiType,
  API_TYPE_LABELS,
  API_TYPE_DEFAULT_URLS,
  API_TYPE_NEEDS_KEY,
} from "../lib/provider-defaults";
import { isTauri, invoke as tauriInvokeV2 } from "../utils/tauri";

// ── i18n translations for the wizard ──

type WizLang = "cn" | "en" | "ja" | "ko" | "de" | "fr" | "es" | "ru" | "th" | "vi";

interface WizText {
  welcome: string;
  subtitle: string;
  step1Title: string;
  step1Sub: string;
  step2Title: string;
  step2Sub: string;
  step3Title: string;
  step3Sub: string;
  step4Title: string;
  step4Sub: string;
  step5Title: string;
  step5Sub: string;
  stepLabels: [string, string, string, string, string];
  next: string;
  prev: string;
  skip: string;
  test: string;
  testing: string;
  connected: string;
  retry: string;
  launch: string;
  launching: string;
  enterConsole: string;
  ready: string;
  readyDesc: string;
  found: string;
  notFound: string;
  migratable: string;
  skipLabel: string;
  envNote: string;
  envNoteMigrate: string;
  bindLabel: string;
  bindDesc: string;
  bindLoopback: string;
  bindAll: string;
  bindCustom: string;
  bindWarn: string;
  portLabel: string;
  langLabel: string;
  configNote: string;
  selectProvider: string;
  selectModel: string;
  enterKey: string;
  channelTitle: string;
  channelSub: string;
  qrScan: string;
  credential: string;
  getQr: string;
  waitingScan: string;
  scanWeChat: string;
  openFeishuAuth: string;
  clickAuth: string;
  writeConfig: string;
  startGateway: string;
  healthCheck: string;
  channelVerify: string;
  modelTest: string;
  migrate: string;
  migrating: string;
  migrateOk: string;
}

const T: Record<WizLang, WizText> = {
  cn: {
    welcome: "\u5F00\u59CB\u8BBE\u7F6E",
    subtitle: "\u8783\u87F9AI\u81EA\u52A8\u5316\u7BA1\u5BB6 \u00B7 Your AI Automation Manager",
    step1Title: "\u68C0\u6D4B\u73AF\u5883",
    step1Sub: "\u68C0\u67E5 rsclaw \u662F\u5426\u5DF2\u5B89\u88C5\uFF0C\u4EE5\u53CA\u662F\u5426\u6709 OpenClaw \u6570\u636E\u53EF\u8FC1\u79FB\u3002",
    step2Title: "\u9009\u62E9 LLM \u63D0\u4F9B\u5546",
    step2Sub: "\u9009\u4E2D\u63D0\u4F9B\u5546\uFF0C\u586B\u5165 API Key\uFF0C\u6D4B\u8BD5\u8FDE\u63A5\u540E\u83B7\u53D6\u53EF\u7528\u6A21\u578B\u5217\u8868\uFF0C\u518D\u9009\u62E9\u6A21\u578B\u3002",
    step3Title: "\u8FDE\u63A5\u6D88\u606F\u901A\u9053",
    step3Sub: "\u9009\u62E9\u8981\u63A5\u5165\u7684\u5E73\u53F0\uFF0C\u652F\u6301\u626B\u7801\u6216\u51ED\u8BC1\u4E24\u79CD\u65B9\u5F0F\u3002",
    step4Title: "\u57FA\u7840\u914D\u7F6E",
    step4Sub: "\u786E\u8BA4\u7F51\u5173\u7AEF\u53E3\u548C\u9ED8\u8BA4\u8BBE\u7F6E\uFF0C\u4FDD\u6301\u9ED8\u8BA4\u5373\u53EF\uFF0C\u540E\u7EED\u53EF\u968F\u65F6\u5728\u914D\u7F6E\u7BA1\u7406\u4E2D\u4FEE\u6539\u3002",
    step5Title: "\u542F\u52A8\u5E76\u9A8C\u8BC1",
    step5Sub: "\u5199\u5165\u914D\u7F6E\u5E76\u542F\u52A8 rsclaw \u7F51\u5173\uFF0C\u81EA\u52A8\u5B8C\u6210\u5065\u5EB7\u68C0\u67E5\u3002",
    stepLabels: ["\u68C0\u6D4B", "\u6A21\u578B", "\u901A\u9053", "\u914D\u7F6E", "\u542F\u52A8"],
    next: "\u4E0B\u4E00\u6B65 \u2192",
    prev: "\u2190 \u4E0A\u4E00\u6B65",
    skip: "\u8DF3\u8FC7\u5411\u5BFC\uFF0C\u624B\u52A8\u914D\u7F6E",
    test: "\u83B7\u53D6\u6A21\u578B",
    testing: "\u83B7\u53D6\u4E2D",
    connected: "\u2713 \u5DF2\u8FDE\u63A5",
    retry: "\u91CD\u65B0\u83B7\u53D6",
    launch: "\u542F\u52A8\u7F51\u5173",
    launching: "\u542F\u52A8\u4E2D",
    enterConsole: "\u8FDB\u5165\u63A7\u5236\u53F0",
    ready: "RsClaw \u5DF2\u5C31\u7EEA",
    readyDesc: "\u7F51\u5173\u8FD0\u884C\u6B63\u5E38\uFF0C\u901A\u9053\u5DF2\u8FDE\u63A5\u3002\u73B0\u5728\u53EF\u4EE5\u5728\u5404\u5E73\u53F0\u4E0A\u548C\u4F60\u7684 AI \u52A9\u624B\u5BF9\u8BDD\u4E86\u3002",
    found: "\u5DF2\u627E\u5230",
    notFound: "\u5F85\u521D\u59CB\u5316",
    migratable: "\u53EF\u8FC1\u79FB",
    skipLabel: "\u8DF3\u8FC7",
    envNote: "\u672A\u68C0\u6D4B\u5230 OpenClaw \u5B89\u88C5\uFF0C\u5C06\u8FDB\u884C\u5168\u65B0\u914D\u7F6E\u3002",
    envNoteMigrate: "\u68C0\u6D4B\u5230 OpenClaw \u5B89\u88C5\uFF0C\u53EF\u5728\u4E0B\u4E00\u6B65\u524D\u8FC1\u79FB\u3002",
    bindLabel: "\u7ED1\u5B9A\u5730\u5740",
    bindDesc: "\u63A7\u5236\u54EA\u4E9B\u7F51\u7EDC\u63A5\u53E3\u53EF\u4EE5\u8BBF\u95EE\u7F51\u5173",
    bindLoopback: "\u2014 \u4EC5\u672C\u673A\u8BBF\u95EE\uFF0C127.0.0.1\uFF08\u63A8\u8350\uFF09",
    bindAll: "\u2014 \u6240\u6709\u7F51\u7EDC\u63A5\u53E3\uFF0C0.0.0.0",
    bindCustom: "\u2014 \u81EA\u5B9A\u4E49 IP \u5730\u5740",
    bindWarn: "\u26A0 \u5C40\u57DF\u7F51\u5185\u6240\u6709\u8BBE\u5907\u5C06\u53EF\u8BBF\u95EE\u7F51\u5173\uFF0C\u8BF7\u786E\u4FDD\u7F51\u7EDC\u73AF\u5883\u5B89\u5168\u3002",
    portLabel: "\u7F51\u5173\u7AEF\u53E3",
    langLabel: "\u754C\u9762\u8BED\u8A00",
    configNote: "\u914D\u7F6E\u5C06\u5199\u5165 ~/.rsclaw/rsclaw.json5\uFF0C\u5411\u5BFC\u5B8C\u6210\u540E\u53EF\u5728\u914D\u7F6E\u7BA1\u7406\u9875\u9762\u7CBE\u7EC6\u8C03\u6574\u3002",
    selectProvider: "\u8BF7\u5148\u586B\u5199 API Key",
    selectModel: "\u9009\u62E9\u6A21\u578B",
    enterKey: "\u8BF7\u5148\u586B\u5199 API Key",
    channelTitle: "\u8FDE\u63A5\u6D88\u606F\u901A\u9053",
    channelSub: "\u9009\u62E9\u8981\u63A5\u5165\u7684\u5E73\u53F0\uFF0C\u652F\u6301\u626B\u7801\u6216\u51ED\u8BC1\u4E24\u79CD\u65B9\u5F0F\u3002",
    qrScan: "\u626B\u7801",
    credential: "\u51ED\u8BC1",
    getQr: "\u83B7\u53D6\u4E8C\u7EF4\u7801",
    waitingScan: "\u7B49\u5F85\u626B\u7801...",
    scanWeChat: "\u4F7F\u7528\u5FAE\u4FE1\u626B\u7801\u767B\u5F55",
    openFeishuAuth: "\u5728\u6D4F\u89C8\u5668\u4E2D\u6253\u5F00\u98DE\u4E66\u6388\u6743",
    clickAuth: "\u70B9\u51FB\u6388\u6743",
    writeConfig: "\u5199\u5165\u914D\u7F6E\u6587\u4EF6",
    startGateway: "\u542F\u52A8\u7F51\u5173\u8FDB\u7A0B",
    healthCheck: "GET /health",
    channelVerify: "\u901A\u9053\u8FDE\u63A5\u9A8C\u8BC1",
    modelTest: "\u6A21\u578B\u63A5\u53E3\u6D4B\u8BD5",
    migrate: "\u8FC1\u79FB\u6570\u636E",
    migrating: "\u8FC1\u79FB\u4E2D...",
    migrateOk: "\u8FC1\u79FB\u6210\u529F\uFF0C\u6B63\u5728\u8FDB\u5165\u63A7\u5236\u53F0...",
  },
  en: {
    welcome: "Get started",
    subtitle: "Your AI Automation Manager",
    step1Title: "Environment check",
    step1Sub: "Check if rsclaw is installed and if there is OpenClaw data to migrate.",
    step2Title: "Choose LLM provider",
    step2Sub: "Select a provider, enter API Key, test connection, then pick a model.",
    step3Title: "Connect channels",
    step3Sub: "Select platforms to connect. QR scan or credential entry supported.",
    step4Title: "Basic configuration",
    step4Sub: "Confirm gateway settings. Defaults are fine. Editable later.",
    step5Title: "Launch & verify",
    step5Sub: "Write config and start rsclaw gateway with health checks.",
    stepLabels: ["Detect", "Model", "Channel", "Config", "Launch"],
    next: "Next \u2192",
    prev: "\u2190 Back",
    skip: "Skip wizard, configure manually",
    test: "Get Models",
    testing: "Fetching",
    connected: "\u2713 OK",
    retry: "Retry",
    launch: "Launch Gateway",
    launching: "Launching",
    enterConsole: "Open Console",
    ready: "RsClaw is Ready",
    readyDesc: "Gateway is running. Channels connected. Start chatting now.",
    found: "Found",
    notFound: "Not found",
    migratable: "Migrate",
    skipLabel: "Skip",
    envNote: "No OpenClaw installation found. Starting fresh.",
    envNoteMigrate: "OpenClaw detected. You can migrate before proceeding.",
    bindLabel: "Bind address",
    bindDesc: "Controls which network interfaces can access the gateway",
    bindLoopback: "-- localhost only, 127.0.0.1 (recommended)",
    bindAll: "-- all interfaces, 0.0.0.0",
    bindCustom: "-- custom IP address",
    bindWarn: "\u26A0 All devices on the network will be able to access the gateway. Ensure the network is secure.",
    portLabel: "Gateway Port",
    langLabel: "Language",
    configNote: "Config will be written to ~/.rsclaw/rsclaw.json5. Fine-tune anytime in Config Manager.",
    selectProvider: "Please enter API Key",
    selectModel: "Select model",
    enterKey: "Please enter API Key",
    channelTitle: "Connect Channels",
    channelSub: "Select platforms to connect. QR scan or credential entry supported.",
    qrScan: "QR Scan",
    credential: "Credentials",
    getQr: "Get QR Code",
    waitingScan: "Waiting...",
    scanWeChat: "Scan with WeChat to login",
    openFeishuAuth: "Open Feishu auth in browser",
    clickAuth: "Click to authorize",
    writeConfig: "Write config file",
    startGateway: "Start gateway",
    healthCheck: "GET /health",
    channelVerify: "Channel verify",
    modelTest: "Model test",
    migrate: "Migrate data",
    migrating: "Migrating...",
    migrateOk: "Migration complete, entering console...",
  },
  ja: {
    welcome: "\u8A2D\u5B9A\u3092\u958B\u59CB",
    subtitle: "\u30DE\u30EB\u30C1\u30A8\u30FC\u30B8\u30A7\u30F3\u30C8AI\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4 \u00B7 \u9AD8\u6027\u80FD\u30A8\u30F3\u30B8\u30F3",
    step1Title: "\u74B0\u5883\u691C\u51FA",
    step1Sub: "rsclaw \u304C\u30A4\u30F3\u30B9\u30C8\u30FC\u30EB\u6E08\u307F\u304B\u78BA\u8A8D\u3057\u307E\u3059\u3002",
    step2Title: "LLM\u30D7\u30ED\u30D0\u30A4\u30C0\u30FC\u9078\u629E",
    step2Sub: "\u30D7\u30ED\u30D0\u30A4\u30C0\u30FC\u3092\u9078\u629E\u3057\u3001API Key\u3092\u5165\u529B\u3057\u3066\u63A5\u7D9A\u30C6\u30B9\u30C8\u5F8C\u3001\u30E2\u30C7\u30EB\u3092\u9078\u629E\u3002",
    step3Title: "\u30C1\u30E3\u30CD\u30EB\u63A5\u7D9A",
    step3Sub: "\u63A5\u7D9A\u3059\u308B\u30D7\u30E9\u30C3\u30C8\u30D5\u30A9\u30FC\u30E0\u3092\u9078\u629E\u3002QR\u30B9\u30AD\u30E3\u30F3\u307E\u305F\u306F\u8A8D\u8A3C\u60C5\u5831\u5165\u529B\u3002",
    step4Title: "\u57FA\u672C\u8A2D\u5B9A",
    step4Sub: "\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u8A2D\u5B9A\u3092\u78BA\u8A8D\u3002\u30C7\u30D5\u30A9\u30EB\u30C8\u3067OK\u3002\u5F8C\u304B\u3089\u5909\u66F4\u53EF\u3002",
    step5Title: "\u8D77\u52D5\u3068\u691C\u8A3C",
    step5Sub: "\u8A2D\u5B9A\u3092\u66F8\u304D\u8FBC\u307F\u3001rsclaw\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u3092\u8D77\u52D5\u3057\u307E\u3059\u3002",
    stepLabels: ["\u691C\u51FA", "\u30E2\u30C7\u30EB", "\u30C1\u30E3\u30CD\u30EB", "\u8A2D\u5B9A", "\u8D77\u52D5"],
    next: "\u6B21\u3078 \u2192",
    prev: "\u2190 \u623B\u308B",
    skip: "\u30A6\u30A3\u30B6\u30FC\u30C9\u3092\u30B9\u30AD\u30C3\u30D7",
    test: "\u30C6\u30B9\u30C8",
    testing: "\u63A5\u7D9A\u4E2D",
    connected: "\u2713 \u63A5\u7D9A\u6E08\u307F",
    retry: "\u518D\u8A66\u884C",
    launch: "\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u8D77\u52D5",
    launching: "\u8D77\u52D5\u4E2D",
    enterConsole: "\u30B3\u30F3\u30BD\u30FC\u30EB\u3078",
    ready: "RsClaw \u6E96\u5099\u5B8C\u4E86",
    readyDesc: "\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u304C\u7A3C\u50CD\u4E2D\u3002\u30C1\u30E3\u30C3\u30C8\u3092\u958B\u59CB\u3067\u304D\u307E\u3059\u3002",
    found: "\u691C\u51FA\u6E08\u307F",
    notFound: "\u672A\u691C\u51FA",
    migratable: "\u79FB\u884C\u53EF",
    skipLabel: "\u30B9\u30AD\u30C3\u30D7",
    envNote: "OpenClaw\u306F\u898B\u3064\u304B\u308A\u307E\u305B\u3093\u3067\u3057\u305F\u3002\u65B0\u898F\u8A2D\u5B9A\u3092\u884C\u3044\u307E\u3059\u3002",
    envNoteMigrate: "OpenClaw\u3092\u691C\u51FA\u3002\u79FB\u884C\u53EF\u80FD\u3067\u3059\u3002",
    bindLabel: "\u30D0\u30A4\u30F3\u30C9\u30A2\u30C9\u30EC\u30B9",
    bindDesc: "\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u3078\u306E\u30A2\u30AF\u30BB\u30B9\u3092\u5236\u5FA1",
    bindLoopback: "-- \u30ED\u30FC\u30AB\u30EB\u306E\u307F, 127.0.0.1 (\u63A8\u5968)",
    bindAll: "-- \u5168\u30A4\u30F3\u30BF\u30FC\u30D5\u30A7\u30FC\u30B9, 0.0.0.0",
    bindCustom: "-- \u30AB\u30B9\u30BF\u30E0IP",
    bindWarn: "\u26A0 \u30CD\u30C3\u30C8\u30EF\u30FC\u30AF\u4E0A\u306E\u5168\u30C7\u30D0\u30A4\u30B9\u304C\u30A2\u30AF\u30BB\u30B9\u53EF\u80FD\u306B\u306A\u308A\u307E\u3059\u3002",
    portLabel: "\u30DD\u30FC\u30C8",
    langLabel: "\u8A00\u8A9E",
    configNote: "\u8A2D\u5B9A\u306F ~/.rsclaw/rsclaw.json5 \u306B\u4FDD\u5B58\u3055\u308C\u307E\u3059\u3002",
    selectProvider: "API Key\u3092\u5165\u529B\u3057\u3066\u304F\u3060\u3055\u3044",
    selectModel: "\u30E2\u30C7\u30EB\u9078\u629E",
    enterKey: "API Key\u3092\u5165\u529B",
    channelTitle: "\u30C1\u30E3\u30CD\u30EB\u63A5\u7D9A",
    channelSub: "\u63A5\u7D9A\u3059\u308B\u30D7\u30E9\u30C3\u30C8\u30D5\u30A9\u30FC\u30E0\u3092\u9078\u629E\u3002",
    qrScan: "QR\u30B9\u30AD\u30E3\u30F3",
    credential: "\u8A8D\u8A3C\u60C5\u5831",
    getQr: "QR\u30B3\u30FC\u30C9\u53D6\u5F97",
    waitingScan: "\u30B9\u30AD\u30E3\u30F3\u5F85\u3061...",
    scanWeChat: "WeChat\u3067\u30B9\u30AD\u30E3\u30F3",
    openFeishuAuth: "\u30D6\u30E9\u30A6\u30B6\u3067\u98DB\u66F8\u8A8D\u8A3C",
    clickAuth: "\u30AF\u30EA\u30C3\u30AF\u3067\u8A8D\u8A3C",
    writeConfig: "\u8A2D\u5B9A\u30D5\u30A1\u30A4\u30EB\u66F8\u304D\u8FBC\u307F",
    startGateway: "\u30B2\u30FC\u30C8\u30A6\u30A7\u30A4\u8D77\u52D5",
    healthCheck: "GET /health",
    channelVerify: "\u30C1\u30E3\u30CD\u30EB\u691C\u8A3C",
    modelTest: "\u30E2\u30C7\u30EB\u30C6\u30B9\u30C8",
    migrate: "\u30C7\u30FC\u30BF\u79FB\u884C",
    migrating: "\u79FB\u884C\u4E2D...",
    migrateOk: "\u79FB\u884C\u5B8C\u4E86\u3001\u30B3\u30F3\u30BD\u30FC\u30EB\u3078...",
  },
  ko: {
    welcome: "\uc2dc\uc791\ud558\uae30",
    subtitle: "\uba40\ud2f0 \uc5d0\uc774\uc804\ud2b8 AI \uac8c\uc774\ud2b8\uc6e8\uc774 \u00B7 \uace0\uc131\ub2a5 \uc5d4\uc9c4",
    step1Title: "\ud658\uacbd \uac10\uc9c0",
    step1Sub: "rsclaw \uc124\uce58 \uc5ec\ubd80\ub97c \ud655\uc778\ud569\ub2c8\ub2e4.",
    step2Title: "LLM \ud504\ub85c\ubc14\uc774\ub354 \uc120\ud0dd",
    step2Sub: "\ud504\ub85c\ubc14\uc774\ub354\ub97c \uc120\ud0dd\ud558\uace0 API Key\ub97c \uc785\ub825\ud558\uc5ec \uc5f0\uacb0\ud558\uc138\uc694.",
    step3Title: "\ucc44\ub110 \uc5f0\uacb0",
    step3Sub: "\uc5f0\uacb0\ud560 \ud50c\ub7ab\ud3fc\uc744 \uc120\ud0dd\ud558\uc138\uc694.",
    step4Title: "\uae30\ubcf8 \uc124\uc815",
    step4Sub: "\uac8c\uc774\ud2b8\uc6e8\uc774 \uc124\uc815\uc744 \ud655\uc778\ud558\uc138\uc694.",
    step5Title: "\uc2dc\uc791 \ubc0f \uac80\uc99d",
    step5Sub: "\uc124\uc815\uc744 \uc800\uc7a5\ud558\uace0 rsclaw \uac8c\uc774\ud2b8\uc6e8\uc774\ub97c \uc2dc\uc791\ud569\ub2c8\ub2e4.",
    stepLabels: ["\uac10\uc9c0", "\ubaa8\ub378", "\ucc44\ub110", "\uc124\uc815", "\uc2dc\uc791"],
    next: "\ub2e4\uc74c \u2192",
    prev: "\u2190 \uc774\uc804",
    skip: "\ub9c8\ubc95\uc0ac \uac74\ub108\ub6f0\uae30",
    test: "\ud14c\uc2a4\ud2b8",
    testing: "\uc5f0\uacb0 \uc911",
    connected: "\u2713 \uc5f0\uacb0\ub428",
    retry: "\uc7ac\uc2dc\ub3c4",
    launch: "\uac8c\uc774\ud2b8\uc6e8\uc774 \uc2dc\uc791",
    launching: "\uc2dc\uc791 \uc911",
    enterConsole: "\ucf58\uc194 \uc5f4\uae30",
    ready: "RsClaw \uc900\ube44 \uc644\ub8cc",
    readyDesc: "\uac8c\uc774\ud2b8\uc6e8\uc774\uac00 \uc2e4\ud589 \uc911\uc785\ub2c8\ub2e4. \ucc44\ud305\uc744 \uc2dc\uc791\ud558\uc138\uc694.",
    found: "\ubc1c\uacac",
    notFound: "\ubbf8\ubc1c\uacac",
    migratable: "\ub9c8\uc774\uadf8\ub808\uc774\uc158",
    skipLabel: "\uac74\ub108\ub6f0\uae30",
    envNote: "OpenClaw\uc744 \ucc3e\uc744 \uc218 \uc5c6\uc2b5\ub2c8\ub2e4. \uc0c8\ub85c \uc124\uc815\ud569\ub2c8\ub2e4.",
    envNoteMigrate: "OpenClaw\uc774 \uac10\uc9c0\ub418\uc5c8\uc2b5\ub2c8\ub2e4. \ub9c8\uc774\uadf8\ub808\uc774\uc158 \uac00\ub2a5.",
    bindLabel: "\ubc14\uc778\ub4dc \uc8fc\uc18c",
    bindDesc: "\uac8c\uc774\ud2b8\uc6e8\uc774 \uc561\uc138\uc2a4 \uc81c\uc5b4",
    bindLoopback: "-- \ub85c\uceec\ub9cc, 127.0.0.1 (\uad8c\uc7a5)",
    bindAll: "-- \ubaa8\ub4e0 \uc778\ud130\ud398\uc774\uc2a4, 0.0.0.0",
    bindCustom: "-- \uc0ac\uc6a9\uc790 \uc815\uc758 IP",
    bindWarn: "\u26A0 \ub124\ud2b8\uc6cc\ud06c\uc758 \ubaa8\ub4e0 \ub514\ubc14\uc774\uc2a4\uac00 \uc561\uc138\uc2a4\ud560 \uc218 \uc788\uc2b5\ub2c8\ub2e4.",
    portLabel: "\ud3ec\ud2b8",
    langLabel: "\uc5b8\uc5b4",
    configNote: "\uc124\uc815\uc740 ~/.rsclaw/rsclaw.json5\uc5d0 \uc800\uc7a5\ub429\ub2c8\ub2e4.",
    selectProvider: "API Key\ub97c \uc785\ub825\ud558\uc138\uc694",
    selectModel: "\ubaa8\ub378 \uc120\ud0dd",
    enterKey: "API Key \uc785\ub825",
    channelTitle: "\ucc44\ub110 \uc5f0\uacb0",
    channelSub: "\uc5f0\uacb0\ud560 \ud50c\ub7ab\ud3fc\uc744 \uc120\ud0dd\ud558\uc138\uc694.",
    qrScan: "QR \uc2a4\uce94",
    credential: "\uc790\uaca9 \uc99d\uba85",
    getQr: "QR \ucf54\ub4dc \ubc1b\uae30",
    waitingScan: "\uc2a4\uce94 \ub300\uae30...",
    scanWeChat: "WeChat\uc73c\ub85c \uc2a4\uce94",
    openFeishuAuth: "\ube44\uc11c \uc778\uc99d \uc5f4\uae30",
    clickAuth: "\ud074\ub9ad\ud558\uc5ec \uc778\uc99d",
    writeConfig: "\uc124\uc815 \ud30c\uc77c \uc4f0\uae30",
    startGateway: "\uac8c\uc774\ud2b8\uc6e8\uc774 \uc2dc\uc791",
    healthCheck: "GET /health",
    channelVerify: "\ucc44\ub110 \uac80\uc99d",
    modelTest: "\ubaa8\ub378 \ud14c\uc2a4\ud2b8",
    migrate: "\ub370\uc774\ud130 \ub9c8\uc774\uadf8\ub808\uc774\uc158",
    migrating: "\ub9c8\uc774\uadf8\ub808\uc774\uc158 \uc911...",
    migrateOk: "\ub9c8\uc774\uadf8\ub808\uc774\uc158 \uc644\ub8cc, \ucf58\uc194\ub85c \uc774\ub3d9...",
  },
  de: {
    welcome: "Einrichtung starten",
    subtitle: "Your AI Automation Manager",
    step1Title: "Umgebung pr\u00FCfen",
    step1Sub: "Pr\u00FCft, ob rsclaw installiert ist.",
    step2Title: "LLM-Anbieter w\u00E4hlen",
    step2Sub: "Anbieter ausw\u00E4hlen, API-Key eingeben, Verbindung testen.",
    step3Title: "Kan\u00E4le verbinden",
    step3Sub: "Plattformen zum Verbinden ausw\u00E4hlen.",
    step4Title: "Grundeinstellungen",
    step4Sub: "Gateway-Einstellungen best\u00E4tigen.",
    step5Title: "Starten & Pr\u00FCfen",
    step5Sub: "Konfiguration schreiben und Gateway starten.",
    stepLabels: ["Erkennung", "Modell", "Kanal", "Konfig", "Start"],
    next: "Weiter \u2192",
    prev: "\u2190 Zur\u00FCck",
    skip: "Assistent \u00FCberspringen",
    test: "Testen",
    testing: "Verbinde",
    connected: "\u2713 Verbunden",
    retry: "Erneut",
    launch: "Gateway starten",
    launching: "Starte",
    enterConsole: "Konsole \u00F6ffnen",
    ready: "RsClaw ist bereit",
    readyDesc: "Gateway l\u00E4uft. Kan\u00E4le verbunden. Jetzt chatten.",
    found: "Gefunden",
    notFound: "Nicht gefunden",
    migratable: "Migrierbar",
    skipLabel: "\u00DCberspringen",
    envNote: "Kein OpenClaw gefunden. Neue Einrichtung.",
    envNoteMigrate: "OpenClaw erkannt. Migration m\u00F6glich.",
    bindLabel: "Bind-Adresse",
    bindDesc: "Steuert den Netzwerkzugang zum Gateway",
    bindLoopback: "-- nur lokal, 127.0.0.1 (empfohlen)",
    bindAll: "-- alle Schnittstellen, 0.0.0.0",
    bindCustom: "-- benutzerdefinierte IP",
    bindWarn: "\u26A0 Alle Ger\u00E4te im Netzwerk k\u00F6nnen auf das Gateway zugreifen.",
    portLabel: "Port",
    langLabel: "Sprache",
    configNote: "Konfiguration wird in ~/.rsclaw/rsclaw.json5 gespeichert.",
    selectProvider: "Bitte API-Key eingeben",
    selectModel: "Modell ausw\u00E4hlen",
    enterKey: "API-Key eingeben",
    channelTitle: "Kan\u00E4le verbinden",
    channelSub: "Plattformen zum Verbinden ausw\u00E4hlen.",
    qrScan: "QR-Scan",
    credential: "Zugangsdaten",
    getQr: "QR-Code abrufen",
    waitingScan: "Warte auf Scan...",
    scanWeChat: "Mit WeChat scannen",
    openFeishuAuth: "Feishu-Auth im Browser \u00F6ffnen",
    clickAuth: "Klicken zur Autorisierung",
    writeConfig: "Konfiguration schreiben",
    startGateway: "Gateway starten",
    healthCheck: "GET /health",
    channelVerify: "Kanal-Pr\u00FCfung",
    modelTest: "Modell-Test",
    migrate: "Daten migrieren",
    migrating: "Migration l\u00E4uft...",
    migrateOk: "Migration abgeschlossen, Konsole wird ge\u00F6ffnet...",
  },
  fr: {
    welcome: "Commencer",
    subtitle: "Your AI Automation Manager",
    step1Title: "V\u00E9rification",
    step1Sub: "V\u00E9rifier si rsclaw est install\u00E9.",
    step2Title: "Choisir un fournisseur LLM",
    step2Sub: "S\u00E9lectionnez un fournisseur, entrez la cl\u00E9 API, testez la connexion.",
    step3Title: "Connecter les canaux",
    step3Sub: "S\u00E9lectionnez les plateformes \u00E0 connecter.",
    step4Title: "Configuration de base",
    step4Sub: "Confirmez les param\u00E8tres de la passerelle.",
    step5Title: "Lancement & V\u00E9rification",
    step5Sub: "\u00C9crire la configuration et d\u00E9marrer la passerelle.",
    stepLabels: ["D\u00E9tection", "Mod\u00E8le", "Canal", "Config", "Lancer"],
    next: "Suivant \u2192",
    prev: "\u2190 Retour",
    skip: "Passer l'assistant",
    test: "Tester",
    testing: "Connexion",
    connected: "\u2713 Connect\u00E9",
    retry: "R\u00E9essayer",
    launch: "D\u00E9marrer la passerelle",
    launching: "D\u00E9marrage",
    enterConsole: "Ouvrir la console",
    ready: "RsClaw est pr\u00EAt",
    readyDesc: "La passerelle fonctionne. Canaux connect\u00E9s. Commencez \u00E0 discuter.",
    found: "Trouv\u00E9",
    notFound: "Non trouv\u00E9",
    migratable: "Migrable",
    skipLabel: "Passer",
    envNote: "Aucune installation OpenClaw trouv\u00E9e. Nouvelle configuration.",
    envNoteMigrate: "OpenClaw d\u00E9tect\u00E9. Migration possible.",
    bindLabel: "Adresse de liaison",
    bindDesc: "Contr\u00F4le l'acc\u00E8s r\u00E9seau \u00E0 la passerelle",
    bindLoopback: "-- local uniquement, 127.0.0.1 (recommand\u00E9)",
    bindAll: "-- toutes les interfaces, 0.0.0.0",
    bindCustom: "-- adresse IP personnalis\u00E9e",
    bindWarn: "\u26A0 Tous les appareils du r\u00E9seau pourront acc\u00E9der \u00E0 la passerelle.",
    portLabel: "Port",
    langLabel: "Langue",
    configNote: "La config sera \u00E9crite dans ~/.rsclaw/rsclaw.json5.",
    selectProvider: "Veuillez entrer la cl\u00E9 API",
    selectModel: "S\u00E9lectionner le mod\u00E8le",
    enterKey: "Entrer la cl\u00E9 API",
    channelTitle: "Connecter les canaux",
    channelSub: "S\u00E9lectionnez les plateformes \u00E0 connecter.",
    qrScan: "Scan QR",
    credential: "Identifiants",
    getQr: "Obtenir le QR",
    waitingScan: "En attente...",
    scanWeChat: "Scanner avec WeChat",
    openFeishuAuth: "Ouvrir l'auth Feishu dans le navigateur",
    clickAuth: "Cliquer pour autoriser",
    writeConfig: "\u00C9crire la configuration",
    startGateway: "D\u00E9marrer la passerelle",
    healthCheck: "GET /health",
    channelVerify: "V\u00E9rification des canaux",
    modelTest: "Test du mod\u00E8le",
    migrate: "Migrer les donn\u00E9es",
    migrating: "Migration en cours...",
    migrateOk: "Migration termin\u00E9e, ouverture de la console...",
  },
  es: {
    welcome: "Comenzar",
    subtitle: "Your AI Automation Manager",
    step1Title: "Verificar entorno",
    step1Sub: "Verificar si rsclaw est\u00E1 instalado.",
    step2Title: "Elegir proveedor LLM",
    step2Sub: "Seleccione un proveedor, ingrese API Key, pruebe la conexi\u00F3n.",
    step3Title: "Conectar canales",
    step3Sub: "Seleccione plataformas para conectar.",
    step4Title: "Configuraci\u00F3n b\u00E1sica",
    step4Sub: "Confirme la configuraci\u00F3n de la pasarela.",
    step5Title: "Iniciar y verificar",
    step5Sub: "Escribir configuraci\u00F3n e iniciar la pasarela.",
    stepLabels: ["Detectar", "Modelo", "Canal", "Config", "Iniciar"],
    next: "Siguiente \u2192",
    prev: "\u2190 Atr\u00E1s",
    skip: "Saltar asistente",
    test: "Probar",
    testing: "Conectando",
    connected: "\u2713 Conectado",
    retry: "Reintentar",
    launch: "Iniciar pasarela",
    launching: "Iniciando",
    enterConsole: "Abrir consola",
    ready: "RsClaw est\u00E1 listo",
    readyDesc: "La pasarela est\u00E1 funcionando. Canales conectados.",
    found: "Encontrado",
    notFound: "No encontrado",
    migratable: "Migrable",
    skipLabel: "Saltar",
    envNote: "No se encontr\u00F3 OpenClaw. Configuraci\u00F3n nueva.",
    envNoteMigrate: "OpenClaw detectado. Migraci\u00F3n posible.",
    bindLabel: "Direcci\u00F3n de enlace",
    bindDesc: "Controla el acceso de red a la pasarela",
    bindLoopback: "-- solo local, 127.0.0.1 (recomendado)",
    bindAll: "-- todas las interfaces, 0.0.0.0",
    bindCustom: "-- IP personalizada",
    bindWarn: "\u26A0 Todos los dispositivos de la red podr\u00E1n acceder.",
    portLabel: "Puerto",
    langLabel: "Idioma",
    configNote: "La config se guardar\u00E1 en ~/.rsclaw/rsclaw.json5.",
    selectProvider: "Ingrese API Key",
    selectModel: "Seleccionar modelo",
    enterKey: "Ingresar API Key",
    channelTitle: "Conectar canales",
    channelSub: "Seleccione plataformas para conectar.",
    qrScan: "Escanear QR",
    credential: "Credenciales",
    getQr: "Obtener QR",
    waitingScan: "Esperando...",
    scanWeChat: "Escanear con WeChat",
    openFeishuAuth: "Abrir auth Feishu en navegador",
    clickAuth: "Clic para autorizar",
    writeConfig: "Escribir configuraci\u00F3n",
    startGateway: "Iniciar pasarela",
    healthCheck: "GET /health",
    channelVerify: "Verificaci\u00F3n de canales",
    modelTest: "Prueba de modelo",
    migrate: "Migrar datos",
    migrating: "Migrando...",
    migrateOk: "Migraci\u00F3n completada, abriendo consola...",
  },
  ru: {
    welcome: "\u041D\u0430\u0447\u0430\u0442\u044C \u043D\u0430\u0441\u0442\u0440\u043E\u0439\u043A\u0443",
    subtitle: "\u041C\u0443\u043B\u044C\u0442\u0438-\u0430\u0433\u0435\u043D\u0442\u043D\u044B\u0439 AI \u0448\u043B\u044E\u0437 \u00B7 \u0412\u044B\u0441\u043E\u043A\u043E\u043F\u0440\u043E\u0438\u0437\u0432\u043E\u0434\u0438\u0442\u0435\u043B\u044C\u043D\u044B\u0439 \u0434\u0432\u0438\u0436\u043E\u043A",
    step1Title: "\u041F\u0440\u043E\u0432\u0435\u0440\u043A\u0430 \u0441\u0440\u0435\u0434\u044B",
    step1Sub: "\u041F\u0440\u043E\u0432\u0435\u0440\u043A\u0430 \u0443\u0441\u0442\u0430\u043D\u043E\u0432\u043A\u0438 rsclaw.",
    step2Title: "\u0412\u044B\u0431\u043E\u0440 \u043F\u0440\u043E\u0432\u0430\u0439\u0434\u0435\u0440\u0430 LLM",
    step2Sub: "\u0412\u044B\u0431\u0435\u0440\u0438\u0442\u0435 \u043F\u0440\u043E\u0432\u0430\u0439\u0434\u0435\u0440\u0430, \u0432\u0432\u0435\u0434\u0438\u0442\u0435 API Key, \u043F\u0440\u043E\u0442\u0435\u0441\u0442\u0438\u0440\u0443\u0439\u0442\u0435.",
    step3Title: "\u041F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u0438\u0435 \u043A\u0430\u043D\u0430\u043B\u043E\u0432",
    step3Sub: "\u0412\u044B\u0431\u0435\u0440\u0438\u0442\u0435 \u043F\u043B\u0430\u0442\u0444\u043E\u0440\u043C\u044B \u0434\u043B\u044F \u043F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u0438\u044F.",
    step4Title: "\u0411\u0430\u0437\u043E\u0432\u0430\u044F \u043D\u0430\u0441\u0442\u0440\u043E\u0439\u043A\u0430",
    step4Sub: "\u041F\u043E\u0434\u0442\u0432\u0435\u0440\u0434\u0438\u0442\u0435 \u043D\u0430\u0441\u0442\u0440\u043E\u0439\u043A\u0438 \u0448\u043B\u044E\u0437\u0430.",
    step5Title: "\u0417\u0430\u043F\u0443\u0441\u043A \u0438 \u043F\u0440\u043E\u0432\u0435\u0440\u043A\u0430",
    step5Sub: "\u0417\u0430\u043F\u0438\u0441\u044C \u043A\u043E\u043D\u0444\u0438\u0433\u0443\u0440\u0430\u0446\u0438\u0438 \u0438 \u0437\u0430\u043F\u0443\u0441\u043A \u0448\u043B\u044E\u0437\u0430.",
    stepLabels: ["\u041E\u0431\u043D\u0430\u0440.", "\u041C\u043E\u0434\u0435\u043B\u044C", "\u041A\u0430\u043D\u0430\u043B", "\u041A\u043E\u043D\u0444\u0438\u0433", "\u0421\u0442\u0430\u0440\u0442"],
    next: "\u0414\u0430\u043B\u0435\u0435 \u2192",
    prev: "\u2190 \u041D\u0430\u0437\u0430\u0434",
    skip: "\u041F\u0440\u043E\u043F\u0443\u0441\u0442\u0438\u0442\u044C \u043C\u0430\u0441\u0442\u0435\u0440",
    test: "\u0422\u0435\u0441\u0442",
    testing: "\u041F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u0438\u0435",
    connected: "\u2713 \u041F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u043E",
    retry: "\u041F\u043E\u0432\u0442\u043E\u0440",
    launch: "\u0417\u0430\u043F\u0443\u0441\u0442\u0438\u0442\u044C \u0448\u043B\u044E\u0437",
    launching: "\u0417\u0430\u043F\u0443\u0441\u043A",
    enterConsole: "\u041E\u0442\u043A\u0440\u044B\u0442\u044C \u043A\u043E\u043D\u0441\u043E\u043B\u044C",
    ready: "RsClaw \u0433\u043E\u0442\u043E\u0432",
    readyDesc: "\u0428\u043B\u044E\u0437 \u0440\u0430\u0431\u043E\u0442\u0430\u0435\u0442. \u041A\u0430\u043D\u0430\u043B\u044B \u043F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u044B.",
    found: "\u041D\u0430\u0439\u0434\u0435\u043D\u043E",
    notFound: "\u041D\u0435 \u043D\u0430\u0439\u0434\u0435\u043D\u043E",
    migratable: "\u041C\u0438\u0433\u0440\u0430\u0446\u0438\u044F",
    skipLabel: "\u041F\u0440\u043E\u043F\u0443\u0441\u0442\u0438\u0442\u044C",
    envNote: "OpenClaw \u043D\u0435 \u043D\u0430\u0439\u0434\u0435\u043D. \u041D\u043E\u0432\u0430\u044F \u043D\u0430\u0441\u0442\u0440\u043E\u0439\u043A\u0430.",
    envNoteMigrate: "OpenClaw \u043E\u0431\u043D\u0430\u0440\u0443\u0436\u0435\u043D. \u041C\u0438\u0433\u0440\u0430\u0446\u0438\u044F \u0432\u043E\u0437\u043C\u043E\u0436\u043D\u0430.",
    bindLabel: "\u0410\u0434\u0440\u0435\u0441 \u043F\u0440\u0438\u0432\u044F\u0437\u043A\u0438",
    bindDesc: "\u0423\u043F\u0440\u0430\u0432\u043B\u0435\u043D\u0438\u0435 \u0441\u0435\u0442\u0435\u0432\u044B\u043C \u0434\u043E\u0441\u0442\u0443\u043F\u043E\u043C \u043A \u0448\u043B\u044E\u0437\u0443",
    bindLoopback: "-- \u0442\u043E\u043B\u044C\u043A\u043E \u043B\u043E\u043A\u0430\u043B\u044C\u043D\u043E, 127.0.0.1 (\u0440\u0435\u043A\u043E\u043C.)",
    bindAll: "-- \u0432\u0441\u0435 \u0438\u043D\u0442\u0435\u0440\u0444\u0435\u0439\u0441\u044B, 0.0.0.0",
    bindCustom: "-- \u043F\u043E\u043B\u044C\u0437\u043E\u0432\u0430\u0442\u0435\u043B\u044C\u0441\u043A\u0438\u0439 IP",
    bindWarn: "\u26A0 \u0412\u0441\u0435 \u0443\u0441\u0442\u0440\u043E\u0439\u0441\u0442\u0432\u0430 \u0432 \u0441\u0435\u0442\u0438 \u0441\u043C\u043E\u0433\u0443\u0442 \u043F\u043E\u0434\u043A\u043B\u044E\u0447\u0438\u0442\u044C\u0441\u044F.",
    portLabel: "\u041F\u043E\u0440\u0442",
    langLabel: "\u042F\u0437\u044B\u043A",
    configNote: "\u041A\u043E\u043D\u0444\u0438\u0433 \u0431\u0443\u0434\u0435\u0442 \u0441\u043E\u0445\u0440\u0430\u043D\u0451\u043D \u0432 ~/.rsclaw/rsclaw.json5.",
    selectProvider: "\u0412\u0432\u0435\u0434\u0438\u0442\u0435 API Key",
    selectModel: "\u0412\u044B\u0431\u0440\u0430\u0442\u044C \u043C\u043E\u0434\u0435\u043B\u044C",
    enterKey: "\u0412\u0432\u0435\u0434\u0438\u0442\u0435 API Key",
    channelTitle: "\u041F\u043E\u0434\u043A\u043B\u044E\u0447\u0435\u043D\u0438\u0435 \u043A\u0430\u043D\u0430\u043B\u043E\u0432",
    channelSub: "\u0412\u044B\u0431\u0435\u0440\u0438\u0442\u0435 \u043F\u043B\u0430\u0442\u0444\u043E\u0440\u043C\u044B.",
    qrScan: "QR-\u0441\u043A\u0430\u043D",
    credential: "\u0423\u0447\u0451\u0442\u043D\u044B\u0435 \u0434\u0430\u043D\u043D\u044B\u0435",
    getQr: "\u041F\u043E\u043B\u0443\u0447\u0438\u0442\u044C QR",
    waitingScan: "\u041E\u0436\u0438\u0434\u0430\u043D\u0438\u0435 \u0441\u043A\u0430\u043D\u0430...",
    scanWeChat: "\u0421\u043A\u0430\u043D\u0438\u0440\u0443\u0439\u0442\u0435 WeChat",
    openFeishuAuth: "\u041E\u0442\u043A\u0440\u044B\u0442\u044C \u0430\u0432\u0442. Feishu \u0432 \u0431\u0440\u0430\u0443\u0437\u0435\u0440\u0435",
    clickAuth: "\u041D\u0430\u0436\u043C\u0438\u0442\u0435 \u0434\u043B\u044F \u0430\u0432\u0442\u043E\u0440\u0438\u0437\u0430\u0446\u0438\u0438",
    writeConfig: "\u0417\u0430\u043F\u0438\u0441\u044C \u043A\u043E\u043D\u0444\u0438\u0433\u0443\u0440\u0430\u0446\u0438\u0438",
    startGateway: "\u0417\u0430\u043F\u0443\u0441\u043A \u0448\u043B\u044E\u0437\u0430",
    healthCheck: "GET /health",
    channelVerify: "\u041F\u0440\u043E\u0432\u0435\u0440\u043A\u0430 \u043A\u0430\u043D\u0430\u043B\u043E\u0432",
    modelTest: "\u0422\u0435\u0441\u0442 \u043C\u043E\u0434\u0435\u043B\u0438",
    migrate: "\u041C\u0438\u0433\u0440\u0430\u0446\u0438\u044F \u0434\u0430\u043D\u043D\u044B\u0445",
    migrating: "\u041C\u0438\u0433\u0440\u0430\u0446\u0438\u044F...",
    migrateOk: "\u041C\u0438\u0433\u0440\u0430\u0446\u0438\u044F \u0437\u0430\u0432\u0435\u0440\u0448\u0435\u043D\u0430, \u043E\u0442\u043A\u0440\u044B\u0432\u0430\u0435\u043C \u043A\u043E\u043D\u0441\u043E\u043B\u044C...",
  },
  th: {
    welcome: "\u0E40\u0E23\u0E34\u0E48\u0E21\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32",
    subtitle: "Your AI Automation Manager",
    step1Title: "\u0E15\u0E23\u0E27\u0E08\u0E2A\u0E2D\u0E1A\u0E2A\u0E20\u0E32\u0E1E\u0E41\u0E27\u0E14\u0E25\u0E49\u0E2D\u0E21",
    step1Sub: "\u0E15\u0E23\u0E27\u0E08\u0E2A\u0E2D\u0E1A\u0E27\u0E48\u0E32 rsclaw \u0E15\u0E34\u0E14\u0E15\u0E31\u0E49\u0E07\u0E41\u0E25\u0E49\u0E27\u0E2B\u0E23\u0E37\u0E2D\u0E44\u0E21\u0E48",
    step2Title: "\u0E40\u0E25\u0E37\u0E2D\u0E01\u0E1C\u0E39\u0E49\u0E43\u0E2B\u0E49\u0E1A\u0E23\u0E34\u0E01\u0E32\u0E23 LLM",
    step2Sub: "\u0E40\u0E25\u0E37\u0E2D\u0E01\u0E1C\u0E39\u0E49\u0E43\u0E2B\u0E49\u0E1A\u0E23\u0E34\u0E01\u0E32\u0E23 \u0E01\u0E23\u0E2D\u0E01 API Key \u0E17\u0E14\u0E2A\u0E2D\u0E1A\u0E01\u0E32\u0E23\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D",
    step3Title: "\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D\u0E0A\u0E48\u0E2D\u0E07\u0E17\u0E32\u0E07",
    step3Sub: "\u0E40\u0E25\u0E37\u0E2D\u0E01\u0E41\u0E1E\u0E25\u0E15\u0E1F\u0E2D\u0E23\u0E4C\u0E21\u0E17\u0E35\u0E48\u0E15\u0E49\u0E2D\u0E07\u0E01\u0E32\u0E23\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D",
    step4Title: "\u0E01\u0E32\u0E23\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32\u0E1E\u0E37\u0E49\u0E19\u0E10\u0E32\u0E19",
    step4Sub: "\u0E22\u0E37\u0E19\u0E22\u0E31\u0E19\u0E01\u0E32\u0E23\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32\u0E40\u0E01\u0E15\u0E40\u0E27\u0E22\u0E4C",
    step5Title: "\u0E40\u0E23\u0E34\u0E48\u0E21\u0E41\u0E25\u0E30\u0E15\u0E23\u0E27\u0E08\u0E2A\u0E2D\u0E1A",
    step5Sub: "\u0E40\u0E02\u0E35\u0E22\u0E19\u0E01\u0E32\u0E23\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32\u0E41\u0E25\u0E30\u0E40\u0E23\u0E34\u0E48\u0E21\u0E40\u0E01\u0E15\u0E40\u0E27\u0E22\u0E4C",
    stepLabels: ["\u0E15\u0E23\u0E27\u0E08", "\u0E42\u0E21\u0E40\u0E14\u0E25", "\u0E0A\u0E48\u0E2D\u0E07", "\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32", "\u0E40\u0E23\u0E34\u0E48\u0E21"],
    next: "\u0E16\u0E31\u0E14\u0E44\u0E1B \u2192",
    prev: "\u2190 \u0E01\u0E25\u0E31\u0E1A",
    skip: "\u0E02\u0E49\u0E32\u0E21\u0E15\u0E31\u0E27\u0E0A\u0E48\u0E27\u0E22",
    test: "\u0E17\u0E14\u0E2A\u0E2D\u0E1A",
    testing: "\u0E01\u0E33\u0E25\u0E31\u0E07\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D",
    connected: "\u2713 \u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D\u0E41\u0E25\u0E49\u0E27",
    retry: "\u0E25\u0E2D\u0E07\u0E43\u0E2B\u0E21\u0E48",
    launch: "\u0E40\u0E23\u0E34\u0E48\u0E21\u0E40\u0E01\u0E15\u0E40\u0E27\u0E22\u0E4C",
    launching: "\u0E01\u0E33\u0E25\u0E31\u0E07\u0E40\u0E23\u0E34\u0E48\u0E21",
    enterConsole: "\u0E40\u0E1B\u0E34\u0E14\u0E04\u0E2D\u0E19\u0E42\u0E0B\u0E25",
    ready: "RsClaw \u0E1E\u0E23\u0E49\u0E2D\u0E21\u0E41\u0E25\u0E49\u0E27",
    readyDesc: "\u0E40\u0E01\u0E15\u0E40\u0E27\u0E22\u0E4C\u0E17\u0E33\u0E07\u0E32\u0E19\u0E2D\u0E22\u0E39\u0E48 \u0E0A\u0E48\u0E2D\u0E07\u0E17\u0E32\u0E07\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D\u0E41\u0E25\u0E49\u0E27",
    found: "\u0E1E\u0E1A\u0E41\u0E25\u0E49\u0E27",
    notFound: "\u0E44\u0E21\u0E48\u0E1E\u0E1A",
    migratable: "\u0E22\u0E49\u0E32\u0E22\u0E44\u0E14\u0E49",
    skipLabel: "\u0E02\u0E49\u0E32\u0E21",
    envNote: "\u0E44\u0E21\u0E48\u0E1E\u0E1A OpenClaw \u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32\u0E43\u0E2B\u0E21\u0E48",
    envNoteMigrate: "\u0E1E\u0E1A OpenClaw \u0E2A\u0E32\u0E21\u0E32\u0E23\u0E16\u0E22\u0E49\u0E32\u0E22\u0E44\u0E14\u0E49",
    bindLabel: "\u0E17\u0E35\u0E48\u0E2D\u0E22\u0E39\u0E48\u0E1C\u0E39\u0E01",
    bindDesc: "\u0E04\u0E27\u0E1A\u0E04\u0E38\u0E21\u0E01\u0E32\u0E23\u0E40\u0E02\u0E49\u0E32\u0E16\u0E36\u0E07\u0E40\u0E04\u0E23\u0E37\u0E2D\u0E02\u0E48\u0E32\u0E22",
    bindLoopback: "-- \u0E40\u0E09\u0E1E\u0E32\u0E30\u0E17\u0E35\u0E48, 127.0.0.1 (\u0E41\u0E19\u0E30\u0E19\u0E33)",
    bindAll: "-- \u0E17\u0E38\u0E01\u0E2D\u0E34\u0E19\u0E40\u0E17\u0E2D\u0E23\u0E4C\u0E40\u0E1F\u0E0B, 0.0.0.0",
    bindCustom: "-- IP \u0E01\u0E33\u0E2B\u0E19\u0E14\u0E40\u0E2D\u0E07",
    bindWarn: "\u26A0 \u0E2D\u0E38\u0E1B\u0E01\u0E23\u0E13\u0E4C\u0E17\u0E31\u0E49\u0E07\u0E2B\u0E21\u0E14\u0E1A\u0E19\u0E40\u0E04\u0E23\u0E37\u0E2D\u0E02\u0E48\u0E32\u0E22\u0E2A\u0E32\u0E21\u0E32\u0E23\u0E16\u0E40\u0E02\u0E49\u0E32\u0E16\u0E36\u0E07\u0E44\u0E14\u0E49",
    portLabel: "\u0E1E\u0E2D\u0E23\u0E4C\u0E15",
    langLabel: "\u0E20\u0E32\u0E29\u0E32",
    configNote: "\u0E01\u0E32\u0E23\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32\u0E08\u0E30\u0E1A\u0E31\u0E19\u0E17\u0E36\u0E01\u0E43\u0E19 ~/.rsclaw/rsclaw.json5",
    selectProvider: "\u0E01\u0E23\u0E38\u0E13\u0E32\u0E01\u0E23\u0E2D\u0E01 API Key",
    selectModel: "\u0E40\u0E25\u0E37\u0E2D\u0E01\u0E42\u0E21\u0E40\u0E14\u0E25",
    enterKey: "\u0E01\u0E23\u0E2D\u0E01 API Key",
    channelTitle: "\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D\u0E0A\u0E48\u0E2D\u0E07\u0E17\u0E32\u0E07",
    channelSub: "\u0E40\u0E25\u0E37\u0E2D\u0E01\u0E41\u0E1E\u0E25\u0E15\u0E1F\u0E2D\u0E23\u0E4C\u0E21\u0E17\u0E35\u0E48\u0E15\u0E49\u0E2D\u0E07\u0E01\u0E32\u0E23\u0E40\u0E0A\u0E37\u0E48\u0E2D\u0E21\u0E15\u0E48\u0E2D",
    qrScan: "QR \u0E2A\u0E41\u0E01\u0E19",
    credential: "\u0E02\u0E49\u0E2D\u0E21\u0E39\u0E25\u0E23\u0E31\u0E1A\u0E23\u0E2D\u0E07",
    getQr: "\u0E23\u0E31\u0E1A QR",
    waitingScan: "\u0E23\u0E2D\u0E2A\u0E41\u0E01\u0E19...",
    scanWeChat: "\u0E2A\u0E41\u0E01\u0E19\u0E14\u0E49\u0E27\u0E22 WeChat",
    openFeishuAuth: "\u0E40\u0E1B\u0E34\u0E14\u0E01\u0E32\u0E23\u0E22\u0E37\u0E19\u0E22\u0E31\u0E19 Feishu",
    clickAuth: "\u0E04\u0E25\u0E34\u0E01\u0E40\u0E1E\u0E37\u0E48\u0E2D\u0E22\u0E37\u0E19\u0E22\u0E31\u0E19",
    writeConfig: "\u0E40\u0E02\u0E35\u0E22\u0E19\u0E01\u0E32\u0E23\u0E15\u0E31\u0E49\u0E07\u0E04\u0E48\u0E32",
    startGateway: "\u0E40\u0E23\u0E34\u0E48\u0E21\u0E40\u0E01\u0E15\u0E40\u0E27\u0E22\u0E4C",
    healthCheck: "GET /health",
    channelVerify: "\u0E15\u0E23\u0E27\u0E08\u0E2A\u0E2D\u0E1A\u0E0A\u0E48\u0E2D\u0E07\u0E17\u0E32\u0E07",
    modelTest: "\u0E17\u0E14\u0E2A\u0E2D\u0E1A\u0E42\u0E21\u0E40\u0E14\u0E25",
    migrate: "\u0E22\u0E49\u0E32\u0E22\u0E02\u0E49\u0E2D\u0E21\u0E39\u0E25",
    migrating: "\u0E01\u0E33\u0E25\u0E31\u0E07\u0E22\u0E49\u0E32\u0E22...",
    migrateOk: "\u0E22\u0E49\u0E32\u0E22\u0E40\u0E2A\u0E23\u0E47\u0E08 \u0E40\u0E02\u0E49\u0E32\u0E2A\u0E39\u0E48\u0E04\u0E2D\u0E19\u0E42\u0E0B\u0E25...",
  },
  vi: {
    welcome: "B\u1EAFt \u0111\u1EA7u thi\u1EBFt l\u1EADp",
    subtitle: "Your AI Automation Manager",
    step1Title: "Ki\u1EC3m tra m\u00F4i tr\u01B0\u1EDDng",
    step1Sub: "Ki\u1EC3m tra xem rsclaw \u0111\u00E3 c\u00E0i \u0111\u1EB7t ch\u01B0a.",
    step2Title: "Ch\u1ECDn nh\u00E0 cung c\u1EA5p LLM",
    step2Sub: "Ch\u1ECDn nh\u00E0 cung c\u1EA5p, nh\u1EADp API Key, ki\u1EC3m tra k\u1EBFt n\u1ED1i.",
    step3Title: "K\u1EBFt n\u1ED1i k\u00EAnh",
    step3Sub: "Ch\u1ECDn n\u1EC1n t\u1EA3ng \u0111\u1EC3 k\u1EBFt n\u1ED1i.",
    step4Title: "C\u1EA5u h\u00ECnh c\u01A1 b\u1EA3n",
    step4Sub: "X\u00E1c nh\u1EADn c\u00E0i \u0111\u1EB7t gateway.",
    step5Title: "Kh\u1EDFi ch\u1EA1y & ki\u1EC3m tra",
    step5Sub: "Ghi c\u1EA5u h\u00ECnh v\u00E0 kh\u1EDFi ch\u1EA1y gateway.",
    stepLabels: ["Ph\u00E1t hi\u1EC7n", "M\u00F4 h\u00ECnh", "K\u00EAnh", "C\u1EA5u h\u00ECnh", "Kh\u1EDFi ch\u1EA1y"],
    next: "Ti\u1EBFp \u2192",
    prev: "\u2190 Quay l\u1EA1i",
    skip: "B\u1ECF qua tr\u1EE3 l\u00FD",
    test: "Ki\u1EC3m tra",
    testing: "\u0110ang k\u1EBFt n\u1ED1i",
    connected: "\u2713 \u0110\u00E3 k\u1EBFt n\u1ED1i",
    retry: "Th\u1EED l\u1EA1i",
    launch: "Kh\u1EDFi ch\u1EA1y gateway",
    launching: "\u0110ang kh\u1EDFi ch\u1EA1y",
    enterConsole: "M\u1EDF b\u1EA3ng \u0111i\u1EC1u khi\u1EC3n",
    ready: "RsClaw \u0111\u00E3 s\u1EB5n s\u00E0ng",
    readyDesc: "Gateway \u0111ang ch\u1EA1y. K\u00EAnh \u0111\u00E3 k\u1EBFt n\u1ED1i.",
    found: "\u0110\u00E3 t\u00ECm th\u1EA5y",
    notFound: "Kh\u00F4ng t\u00ECm th\u1EA5y",
    migratable: "C\u00F3 th\u1EC3 di chuy\u1EC3n",
    skipLabel: "B\u1ECF qua",
    envNote: "Kh\u00F4ng t\u00ECm th\u1EA5y OpenClaw. Thi\u1EBFt l\u1EADp m\u1EDBi.",
    envNoteMigrate: "Ph\u00E1t hi\u1EC7n OpenClaw. C\u00F3 th\u1EC3 di chuy\u1EC3n.",
    bindLabel: "\u0110\u1ECBa ch\u1EC9 bind",
    bindDesc: "Ki\u1EC3m so\u00E1t truy c\u1EADp m\u1EA1ng v\u00E0o gateway",
    bindLoopback: "-- ch\u1EC9 local, 127.0.0.1 (khuy\u1EBFn ngh\u1ECB)",
    bindAll: "-- t\u1EA5t c\u1EA3 giao di\u1EC7n, 0.0.0.0",
    bindCustom: "-- IP t\u00F9y ch\u1EC9nh",
    bindWarn: "\u26A0 T\u1EA5t c\u1EA3 thi\u1EBFt b\u1ECB tr\u00EAn m\u1EA1ng s\u1EBD c\u00F3 th\u1EC3 truy c\u1EADp.",
    portLabel: "C\u1ED5ng",
    langLabel: "Ng\u00F4n ng\u1EEF",
    configNote: "C\u1EA5u h\u00ECnh s\u1EBD \u0111\u01B0\u1EE3c l\u01B0u v\u00E0o ~/.rsclaw/rsclaw.json5.",
    selectProvider: "Nh\u1EADp API Key",
    selectModel: "Ch\u1ECDn m\u00F4 h\u00ECnh",
    enterKey: "Nh\u1EADp API Key",
    channelTitle: "K\u1EBFt n\u1ED1i k\u00EAnh",
    channelSub: "Ch\u1ECDn n\u1EC1n t\u1EA3ng \u0111\u1EC3 k\u1EBFt n\u1ED1i.",
    qrScan: "Qu\u00E9t QR",
    credential: "Th\u00F4ng tin x\u00E1c th\u1EF1c",
    getQr: "L\u1EA5y m\u00E3 QR",
    waitingScan: "\u0110ang ch\u1EDD qu\u00E9t...",
    scanWeChat: "Qu\u00E9t b\u1EB1ng WeChat",
    openFeishuAuth: "M\u1EDF x\u00E1c th\u1EF1c Feishu",
    clickAuth: "Nh\u1EA5p \u0111\u1EC3 x\u00E1c th\u1EF1c",
    writeConfig: "Ghi c\u1EA5u h\u00ECnh",
    startGateway: "Kh\u1EDFi ch\u1EA1y gateway",
    healthCheck: "GET /health",
    channelVerify: "Ki\u1EC3m tra k\u00EAnh",
    modelTest: "Ki\u1EC3m tra m\u00F4 h\u00ECnh",
    migrate: "Di chuy\u1EC3n d\u1EEF li\u1EC7u",
    migrating: "\u0110ang di chuy\u1EC3n...",
    migrateOk: "Di chuy\u1EC3n ho\u00E0n t\u1EA5t, m\u1EDF b\u1EA3ng \u0111i\u1EC1u khi\u1EC3n...",
  },
};

// Language selection grid for the welcome screen (2 cols x 5 rows = 10 langs)
const LANG_GRID: { code: WizLang; label: string }[] = [
  { code: "cn", label: "\u7B80\u4F53\u4E2D\u6587" },
  { code: "en", label: "English" },
  { code: "ja", label: "\u65E5\u672C\u8A9E" },
  { code: "ko", label: "\uD55C\uAD6D\uC5B4" },
  { code: "th", label: "\u0E44\u0E17\u0E22" },
  { code: "vi", label: "Ti\u1EBFng Vi\u1EC7t" },
  { code: "fr", label: "Fran\u00E7ais" },
  { code: "de", label: "Deutsch" },
  { code: "es", label: "Espa\u00F1ol" },
  { code: "ru", label: "\u0420\u0443\u0441\u0441\u043A\u0438\u0439" },
];

// Map WizLang to gateway.language config value (matches resolve_lang in i18n.rs)
const LANG_TO_CONFIG: Record<WizLang, string> = {
  cn: "Chinese", en: "English", ja: "Japanese", ko: "Korean",
  th: "Thai", vi: "Vietnamese", fr: "French", de: "German",
  es: "Spanish", ru: "Russian",
};

// Reverse mapping: config value → WizLang
const CONFIG_TO_LANG: Record<string, WizLang> = Object.fromEntries(
  Object.entries(LANG_TO_CONFIG).map(([k, v]) => [v, k as WizLang])
) as Record<string, WizLang>;

function detectWizLang(): WizLang {
  // Check localStorage first
  try {
    const saved = localStorage.getItem("rsclaw-lang");
    if (saved && saved in T) return saved as WizLang;
  } catch {}
  // Detect from navigator.language
  try {
    const nav = navigator.language.toLowerCase();
    if (nav.startsWith("zh")) return "cn";
    if (nav.startsWith("ja")) return "ja";
    if (nav.startsWith("ko")) return "ko";
    if (nav.startsWith("th")) return "th";
    if (nav.startsWith("vi")) return "vi";
    if (nav.startsWith("fr")) return "fr";
    if (nav.startsWith("de")) return "de";
    if (nav.startsWith("es")) return "es";
    if (nav.startsWith("ru")) return "ru";
    if (nav.startsWith("en")) return "en";
  } catch {}
  return "cn"; // default Chinese
}

// ── Data ──

export interface ProviderDef {
  id: string;
  name: string;
  tag: string;
  tagEn: string;
  keyLabel: string;
  keyPlaceholder: string;
  isUrl?: boolean;
  sep?: boolean;
  hasBaseUrl?: boolean;
  defaultBaseUrl?: string;
  defaultUserAgent?: string;
}

// All providers (unordered lookup table)
export const ALL_PROVIDERS: Record<string, ProviderDef> = {
  qwen:        { id: "qwen",        name: "Qwen (\u5343\u95EE)", tag: "\u56FD\u5185\u76F4\u8FDE",      tagEn: "China direct",      keyLabel: "DashScope API Key",   keyPlaceholder: "sk-..." },
  doubao:      { id: "doubao",      name: "Doubao (\u8C46\u5305)", tag: "\u5B57\u8282\u8DF3\u52A8",     tagEn: "ByteDance",         keyLabel: "ARK API Key",         keyPlaceholder: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx", hasBaseUrl: true, defaultBaseUrl: "https://ark.cn-beijing.volces.com/api/v3" },
  minimax:     { id: "minimax",     name: "MiniMax",            tag: "\u56FD\u5185",                  tagEn: "China",             keyLabel: "MiniMax API Key",     keyPlaceholder: "eyJ..." },
  deepseek:    { id: "deepseek",    name: "DeepSeek",           tag: "\u4F4E\u6210\u672C",            tagEn: "Low cost",          keyLabel: "DeepSeek API Key",    keyPlaceholder: "sk-..." },
  kimi:        { id: "kimi",        name: "Kimi",               tag: "\u56FD\u5185",                  tagEn: "China",             keyLabel: "Kimi API Key",        keyPlaceholder: "sk-...", hasBaseUrl: true, defaultBaseUrl: "https://api.moonshot.cn/v1", defaultUserAgent: "claude-code/0.1.0" },
  codingplan:  { id: "codingplan",  name: "CodingPlan",         tag: "\u7F16\u7A0B\u8BA1\u5212",      tagEn: "Coding Plan",       keyLabel: "API URL",             keyPlaceholder: "https://api.example.com/v1", isUrl: true },
  zhipu:       { id: "zhipu",       name: "Zhipu (GLM)",        tag: "\u56FD\u5185",                  tagEn: "China",             keyLabel: "Zhipu API Key",       keyPlaceholder: "sk-..." },
  ollama:      { id: "ollama",      name: "Ollama (local)",     tag: "\u65E0\u9700 Key",              tagEn: "No Key",            keyLabel: "URL",                 keyPlaceholder: "http://localhost:11434/v1", isUrl: true },
  custom:      { id: "custom",      name: "Custom Provider",    tag: "\u81EA\u5B9A\u4E49",            tagEn: "Custom",            keyLabel: "API URL",             keyPlaceholder: "https://api.example.com/v1", isUrl: true },
  gaterouter:  { id: "gaterouter",  name: "GateRouter",         tag: "\u805A\u5408\u8DEF\u7531",      tagEn: "Aggregator",        keyLabel: "GateRouter Key",      keyPlaceholder: "sk-..." },
  openrouter:  { id: "openrouter",  name: "OpenRouter",         tag: "\u805A\u5408\u8DEF\u7531",      tagEn: "Aggregator",        keyLabel: "OpenRouter Key",      keyPlaceholder: "sk-or-..." },
  anthropic:   { id: "anthropic",   name: "Anthropic (Claude)", tag: "\u63A8\u8350",                  tagEn: "Recommended",       keyLabel: "Anthropic API Key",   keyPlaceholder: "sk-ant-..." },
  openai:      { id: "openai",      name: "OpenAI (GPT)",       tag: "",                              tagEn: "",                  keyLabel: "OpenAI API Key",      keyPlaceholder: "sk-..." },
  gemini:      { id: "gemini",      name: "Google Gemini",      tag: "",                              tagEn: "",                  keyLabel: "Gemini API Key",      keyPlaceholder: "AIza..." },
  grok:        { id: "grok",        name: "xAI (Grok)",         tag: "",                              tagEn: "",                  keyLabel: "xAI API Key",         keyPlaceholder: "xai-..." },
  groq:        { id: "groq",        name: "Groq",               tag: "\u5FEB\u901F\u63A8\u7406",      tagEn: "Fast",              keyLabel: "Groq API Key",        keyPlaceholder: "gsk_..." },
  siliconflow: { id: "siliconflow", name: "SiliconFlow",        tag: "\u56FD\u5185\u52A0\u901F",      tagEn: "China accel",       keyLabel: "SiliconFlow Key",     keyPlaceholder: "sk-..." },
};

export const PROV_ORDER_ZH = ["doubao","qwen","custom","codingplan","minimax","deepseek","kimi","zhipu","ollama","gaterouter","openrouter","anthropic","openai","gemini","grok","groq","siliconflow"];
export const PROV_ORDER_EN = ["anthropic","openai","gemini","grok","openrouter","ollama","custom","codingplan","groq","doubao","qwen","minimax","deepseek","kimi","zhipu","gaterouter","siliconflow"];

function getProviders(lang?: string): ProviderDef[] {
  const isZhOrder = (lang || getLang()) === "cn";
  const order = isZhOrder ? PROV_ORDER_ZH : PROV_ORDER_EN;
  return order.map((id) => ALL_PROVIDERS[id]).filter(Boolean);
}

interface ModelDef {
  id: string;
  tag: string;
  tagEn: string;
  rec: boolean;
}

export const MODELS: Record<string, ModelDef[]> = {
  qwen: [
    { id: "qwen3.6-plus", tag: "\u6700\u65B0", tagEn: "Latest", rec: true },
    { id: "qwen-max", tag: "\u63A8\u8350", tagEn: "Recommended", rec: false },
    { id: "qwen-plus", tag: "\u5747\u8861", tagEn: "Balanced", rec: false },
    { id: "qwen-turbo", tag: "\u5FEB\u901F", tagEn: "Fast", rec: false },
  ],
  doubao: [
    { id: "doubao-seed-2-0-pro-260215", tag: "\u63A8\u8350", tagEn: "Recommended", rec: true },
    { id: "doubao-1-5-pro-256k-250115", tag: "\u957F\u6587\u672C", tagEn: "Long context", rec: false },
  ],
  deepseek: [
    { id: "deepseek-chat", tag: "\u901A\u7528", tagEn: "General", rec: true },
    { id: "deepseek-reasoner", tag: "\u63A8\u7406", tagEn: "Reasoning", rec: false },
  ],
  anthropic: [
    { id: "claude-sonnet-4-20250514", tag: "\u63A8\u8350", tagEn: "Recommended", rec: true },
    { id: "claude-opus-4-5", tag: "\u6700\u5F3A", tagEn: "Strongest", rec: false },
    { id: "claude-haiku-4-5-20251001", tag: "\u6700\u5FEB", tagEn: "Fastest", rec: false },
  ],
  openai: [
    { id: "gpt-4o", tag: "\u63A8\u8350", tagEn: "Recommended", rec: true },
    { id: "gpt-4o-mini", tag: "\u5FEB\u901F", tagEn: "Fast", rec: false },
    { id: "o3", tag: "\u63A8\u7406", tagEn: "Reasoning", rec: false },
  ],
  groq: [
    { id: "llama-3.3-70b-versatile", tag: "\u63A8\u8350", tagEn: "Recommended", rec: true },
    { id: "mixtral-8x7b-32768", tag: "", tagEn: "", rec: false },
  ],
  ollama: [
    { id: "llama3.2:3b", tag: "\u5DF2\u5B89\u88C5", tagEn: "Installed", rec: true },
    { id: "qwen2.5:7b", tag: "\u5DF2\u5B89\u88C5", tagEn: "Installed", rec: false },
  ],
};

export interface ChannelDef {
  id: string;
  icon: string;
  name: string;
  nameEn: string;
  hasQr: boolean;
  qrLabel?: string;
  qrLabelEn?: string;
  credFields: { key: string; label: string; type: string; ph: string }[];
}

// All channels (lookup)
export const ALL_CHANNELS: Record<string, ChannelDef> = {
  feishu:   { id: "feishu",   icon: "\u98DE", name: "\u98DE\u4E66 / Lark", nameEn: "Feishu / Lark",  hasQr: true, qrLabel: "\u626B\u7801", qrLabelEn: "QR", credFields: [
    { key: "appId", label: "App ID", type: "text", ph: "cli_xxx" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  wechat:   { id: "wechat",   icon: "\u5FAE", name: "\u5FAE\u4FE1",        nameEn: "Weixin",         hasQr: true, qrLabel: "\u626B\u7801", qrLabelEn: "QR", credFields: [] },
  wecom:    { id: "wecom",    icon: "WC",     name: "\u4F01\u4E1A\u5FAE\u4FE1", nameEn: "WeCom",     hasQr: false, credFields: [
    { key: "botId", label: "Bot ID", type: "text", ph: "" },
    { key: "secret", label: "Secret", type: "password", ph: "" },
  ] },
  qq:       { id: "qq",       icon: "QQ",     name: "QQ Bot",              nameEn: "QQ Bot",          hasQr: false, credFields: [
    { key: "appId", label: "App ID", type: "text", ph: "" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  dingtalk: { id: "dingtalk", icon: "DT",     name: "\u9489\u9489",        nameEn: "DingTalk",        hasQr: false, credFields: [
    { key: "appKey", label: "App Key", type: "text", ph: "" },
    { key: "appSecret", label: "App Secret", type: "password", ph: "" },
  ] },
  telegram: { id: "telegram", icon: "Tg",     name: "Telegram",            nameEn: "Telegram",        hasQr: false, credFields: [
    { key: "botToken", label: "Bot Token", type: "password", ph: "123456:ABC-DEF..." },
  ] },
  matrix:   { id: "matrix",   icon: "Mx",     name: "Matrix",              nameEn: "Matrix",          hasQr: false, credFields: [
    { key: "homeserver", label: "Homeserver", type: "text", ph: "https://matrix.org" },
    { key: "userId", label: "User ID", type: "text", ph: "@bot:matrix.org" },
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  discord:  { id: "discord",  icon: "Dc",     name: "Discord",             nameEn: "Discord",         hasQr: false, credFields: [
    { key: "token", label: "Bot Token", type: "password", ph: "" },
  ] },
  slack:    { id: "slack",    icon: "Sl",     name: "Slack",               nameEn: "Slack",           hasQr: false, credFields: [
    { key: "botToken", label: "Bot Token", type: "password", ph: "xoxb-..." },
    { key: "appToken", label: "App Token", type: "password", ph: "xapp-..." },
  ] },
  whatsapp: { id: "whatsapp", icon: "WA",     name: "WhatsApp",            nameEn: "WhatsApp",        hasQr: false, credFields: [
    { key: "phoneNumberId", label: "Phone Number ID", type: "text", ph: "" },
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  signal:   { id: "signal",   icon: "Sg",     name: "Signal",              nameEn: "Signal",          hasQr: false, credFields: [
    { key: "phone", label: "Phone Number", type: "text", ph: "+1234567890" },
  ] },
  line:     { id: "line",     icon: "Li",     name: "LINE",                nameEn: "LINE",            hasQr: false, credFields: [
    { key: "channelSecret", label: "Channel Secret", type: "password", ph: "" },
    { key: "channelAccessToken", label: "Access Token", type: "password", ph: "" },
  ] },
  zalo:     { id: "zalo",     icon: "Za",     name: "Zalo",                nameEn: "Zalo",            hasQr: false, credFields: [
    { key: "accessToken", label: "Access Token", type: "password", ph: "" },
    { key: "oaSecret", label: "OA Secret", type: "password", ph: "" },
  ] },
};

export const CH_ORDER_ZH = ["feishu","wechat","wecom","qq","dingtalk","telegram","matrix","discord","slack","whatsapp","signal","line","zalo"];
export const CH_ORDER_EN = ["telegram","matrix","discord","slack","whatsapp","signal","line","feishu","wechat","wecom","qq","dingtalk","zalo"];

function getChannels(lang?: string): ChannelDef[] {
  const isZhOrder = (lang || getLang()) === "cn";
  const order = isZhOrder ? CH_ORDER_ZH : CH_ORDER_EN;
  return order.map((id) => ALL_CHANNELS[id]).filter(Boolean);
}

// (step labels are now in T[lang].stepLabels)

// ── Inline Styles ──

const V = {
  bg0: "#080809",
  bg1: "#0f1013",
  bg2: "#141618",
  bg3: "#1a1c22",
  bg4: "#1f2126",
  bg5: "#252830",
  bd: "rgba(255,255,255,.055)",
  bd2: "rgba(255,255,255,.09)",
  bd3: "rgba(255,255,255,.14)",
  t0: "#eceaf4",
  t1: "#9896a4",
  t2: "#4a4858",
  t3: "#2e2c3a",
  or: "#f97316",
  or2: "#fb923c",
  olo: "rgba(249,115,22,.1)",
  obrd: "rgba(249,115,22,.22)",
  green: "#2dd4a0",
  glo: "rgba(45,212,160,.08)",
  gbrd: "rgba(45,212,160,.18)",
  red: "#d95f5f",
  rlo: "rgba(217,95,95,.08)",
  rbrd: "rgba(217,95,95,.18)",
  sans: "'Geist', sans-serif",
  mono: "'JetBrains Mono', monospace",
};

const S = {
  page: {
    position: "fixed" as const,
    inset: 0,
    zIndex: 200,
    background: V.bg1,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    overflowY: "auto" as const,
    fontSize: 13,
    fontFamily: V.sans,
  },
  window: {
    width: "100%",
    maxWidth: 520,
    margin: "auto",
    padding: "48px 20px",
    background: V.bg1,
    borderRadius: 16,
    overflow: "hidden",
    border: `1px solid ${V.bd}`,
    boxShadow: "0 24px 64px rgba(0,0,0,.6)",
  },
  pbar: { height: 2, background: V.bg3 },
  pfill: (pct: number) => ({
    height: 2,
    background: V.or,
    width: `${pct}%`,
    transition: "width 0.4s ease",
    borderRadius: 1,
  }),
  wiz: {
    padding: "24px 32px 24px",
    display: "flex",
    flexDirection: "column" as const,
    alignItems: "center",
  },
  logoWrap: {
    display: "flex",
    flexDirection: "column" as const,
    alignItems: "center",
    gap: 8,
    marginBottom: 32,
  },
  logoBox: {
    width: 52,
    height: 52,
    borderRadius: 15,
    background: V.or,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    boxShadow: "0 0 0 1px rgba(249,115,22,.4)",
  },
  logoName: { fontSize: 20, fontWeight: 800, color: V.t0, letterSpacing: -0.5 },
  logoSub: { fontSize: 11, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3 },
  steps: {
    display: "flex",
    alignItems: "flex-start",
    width: "100%",
    marginBottom: 32,
  },
  stp: (isLast: boolean) => ({
    display: "flex",
    alignItems: "flex-start",
    flex: isLast ? 0 : 1,
  }),
  stepUnit: {
    display: "flex",
    flexDirection: "column" as const,
    alignItems: "center",
    gap: 6,
    flexShrink: 0,
  },
  sc: (state: string) => ({
    width: 28,
    height: 28,
    borderRadius: "50%",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 11,
    fontWeight: 700,
    fontFamily: V.mono,
    flexShrink: 0,
    transition: "all 0.2s",
    ...(state === "done"
      ? { background: V.or, color: "#fff" }
      : state === "active"
        ? { background: V.or, color: "#fff", boxShadow: "0 0 0 4px rgba(249,115,22,.18)" }
        : { background: V.bg3, color: V.t3, border: `1.5px solid ${V.bg5}` }),
  }),
  sl: (done: boolean) => ({
    flex: 1,
    height: 1.5,
    margin: "0 4px",
    marginTop: 14,
    alignSelf: "flex-start" as const,
    transition: "background 0.25s",
    background: done ? V.or : V.bg4,
    opacity: done ? 0.5 : 1,
  }),
  slbl: (state: string) => ({
    fontSize: 9,
    fontWeight: 500,
    letterSpacing: 0.3,
    whiteSpace: "nowrap" as const,
    textAlign: "center" as const,
    color: state === "active" ? V.or : state === "done" ? "rgba(249,115,22,.5)" : V.t3,
  }),
  card: {
    width: "100%",
    background: V.bg2,
    border: `1px solid ${V.bd}`,
    borderRadius: 13,
    padding: 24,
    display: "flex",
    flexDirection: "column" as const,
  },
  cardTitle: { fontSize: 17, fontWeight: 700, color: V.t0, letterSpacing: -0.4, marginBottom: 5 },
  cardSub: { fontSize: 12, color: V.t2, lineHeight: 1.6, marginBottom: 20 },
  navRow: { display: "flex", justifyContent: "space-between", marginTop: 24, alignItems: "center", paddingBottom: 4 },
  btnPrev: {
    padding: "8px 16px",
    borderRadius: 8,
    border: `1px solid ${V.bd2}`,
    background: V.bg2,
    color: V.t2,
    fontSize: 12,
    fontWeight: 500,
    cursor: "pointer",
    fontFamily: "inherit",
    transition: "all .12s",
  },
  btnNext: {
    padding: "9px 22px",
    borderRadius: 8,
    border: "none",
    background: V.or,
    color: "#fff",
    fontSize: 12,
    fontWeight: 700,
    cursor: "pointer",
    fontFamily: "inherit",
    boxShadow: "0 2px 10px rgba(249,115,22,.3)",
    transition: "all .13s",
  },
  btnNextDisabled: {
    padding: "9px 22px",
    borderRadius: 8,
    border: "none",
    background: V.or,
    color: "#fff",
    fontSize: 12,
    fontWeight: 700,
    cursor: "not-allowed",
    fontFamily: "inherit",
    opacity: 0.4,
    boxShadow: "none",
  },
  skipLink: {
    fontSize: 11,
    color: V.t3,
    cursor: "pointer",
    background: "none",
    border: "none",
    fontFamily: "inherit",
  },
  // Step 1
  detRow: {
    display: "flex",
    alignItems: "center",
    gap: 12,
    padding: "12px 14px",
    background: V.bg3,
    border: `1px solid ${V.bd}`,
    borderRadius: 9,
    marginBottom: 8,
  },
  pillOk: {
    fontSize: 10,
    padding: "2px 9px",
    borderRadius: 20,
    fontFamily: V.mono,
    fontWeight: 500,
    background: V.glo,
    color: V.green,
    border: `1px solid ${V.gbrd}`,
  },
  pillWarn: {
    fontSize: 10,
    padding: "2px 9px",
    borderRadius: 20,
    fontFamily: V.mono,
    fontWeight: 500,
    background: V.olo,
    color: V.or,
    border: `1px solid ${V.obrd}`,
  },
  pillSkip: {
    fontSize: 10,
    padding: "2px 9px",
    borderRadius: 20,
    fontFamily: V.mono,
    fontWeight: 500,
    background: V.bg4,
    color: V.t2,
    border: `1px solid ${V.bd2}`,
  },
  infoNote: {
    display: "flex",
    alignItems: "flex-start",
    gap: 8,
    padding: "10px 13px",
    borderRadius: 8,
    marginTop: 12,
    background: V.olo,
    border: `1px solid ${V.obrd}`,
    fontSize: 11.5,
    color: "#b07238",
    lineHeight: 1.55,
  },
  // Step 2: providers
  provList: { display: "flex", flexDirection: "column" as const, gap: 8 },
  prov: (selected: boolean) => ({
    border: `1.5px solid ${selected ? V.or : V.bd2}`,
    borderRadius: 10,
    overflow: "hidden",
    transition: "border-color .13s",
    background: V.bg3,
  }),
  provHead: {
    display: "flex",
    alignItems: "center",
    gap: 11,
    padding: "13px 14px",
    cursor: "pointer",
  },
  provIco: (selected: boolean, color: string) => ({
    width: 30,
    height: 30,
    borderRadius: 8,
    background: selected ? "rgba(249,115,22,.15)" : V.bg4,
    border: `1px solid ${selected ? "rgba(249,115,22,.25)" : V.bd2}`,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 11,
    fontWeight: 700,
    color: selected ? V.or : color,
    flexShrink: 0,
    transition: "all .13s",
  }),
  provName: (selected: boolean) => ({
    fontSize: 13,
    fontWeight: 600,
    color: selected ? V.t0 : V.t1,
  }),
  provDesc: { fontSize: 10, color: V.t3, marginTop: 2 },
  provCheck: (selected: boolean) => ({
    width: 18,
    height: 18,
    borderRadius: "50%",
    border: `1.5px solid ${selected ? V.or : V.bd3}`,
    background: selected ? V.or : "transparent",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 10,
    color: selected ? "#fff" : "transparent",
    flexShrink: 0,
    transition: "all .13s",
  }),
  provBody: (open: boolean) => ({
    padding: open ? "0 14px 14px" : "0 14px",
    maxHeight: open ? 400 : 0,
    overflow: "hidden",
    transition: "max-height .25s ease, padding .25s ease",
  }),
  provSep: { height: 1, background: V.bd, marginBottom: 14 },
  fLabel: {
    fontSize: 10,
    color: V.t3,
    letterSpacing: 0.4,
    marginBottom: 5,
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    fontFamily: V.mono,
  },
  fRow: { display: "flex", gap: 8, marginBottom: 10 },
  fInput: (state?: "ok" | "err") => ({
    flex: 1,
    background: V.bg4,
    border: `1px solid ${state === "ok" ? V.green : state === "err" ? V.red : V.bd2}`,
    borderRadius: 7,
    padding: "8px 10px",
    color: V.t0,
    fontFamily: V.mono,
    fontSize: 11.5,
    outline: "none",
    transition: "border-color .12s",
  }),
  btnTest: (state: "idle" | "testing" | "success" | "error") => ({
    padding: "8px 14px",
    borderRadius: 7,
    border: `1px solid ${state === "success" ? V.gbrd : state === "error" ? V.rbrd : state === "testing" ? V.obrd : V.bd2}`,
    background: state === "success" ? V.glo : state === "error" ? V.rlo : V.bg4,
    color: state === "success" ? V.green : state === "error" ? V.red : state === "testing" ? V.or : V.t1,
    fontSize: 11,
    fontWeight: 500,
    cursor: "pointer",
    fontFamily: "inherit",
    transition: "all .13s",
    whiteSpace: "nowrap" as const,
    flexShrink: 0,
    display: "inline-flex",
    alignItems: "center",
    gap: 5,
  }),
  modelList: { marginTop: 10, display: "flex", flexDirection: "column" as const, gap: 5 },
  modelItem: (selected: boolean) => ({
    display: "flex",
    alignItems: "center",
    gap: 10,
    padding: "8px 11px",
    borderRadius: 7,
    cursor: "pointer",
    border: `1px solid ${selected ? V.obrd : "transparent"}`,
    background: selected ? V.olo : "transparent",
    transition: "all .12s",
  }),
  modelRadio: (selected: boolean) => ({
    width: 15,
    height: 15,
    borderRadius: "50%",
    border: `1.5px solid ${selected ? V.or : V.bd3}`,
    background: selected ? V.or : "transparent",
    flexShrink: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    transition: "all .13s",
  }),
  modelRadioDot: (selected: boolean) => ({
    width: 6,
    height: 6,
    borderRadius: "50%",
    background: "#fff",
    opacity: selected ? 1 : 0,
    transition: "opacity .13s",
  }),
  modelId: (selected: boolean) => ({
    fontFamily: V.mono,
    fontSize: 11,
    flex: 1,
    color: selected ? V.t0 : V.t1,
  }),
  modelTag: (rec: boolean) => ({
    fontSize: 9,
    padding: "1px 6px",
    borderRadius: 3,
    fontWeight: 500,
    background: rec ? V.olo : V.bg4,
    color: rec ? V.or : V.t2,
  }),
  recBadge: {
    fontSize: 9,
    color: V.or,
    background: V.olo,
    padding: "1px 6px",
    borderRadius: 3,
    marginLeft: 4,
    fontWeight: 600,
  },
  // Step 3: channels
  chList: { display: "flex", flexDirection: "column" as const, gap: 8 },
  chCard: (enabled: boolean) => ({
    border: `1.5px solid ${enabled ? V.or : V.bd2}`,
    borderRadius: 10,
    overflow: "hidden",
    background: V.bg3,
    transition: "border-color .13s",
  }),
  chHead: {
    display: "flex",
    alignItems: "center",
    gap: 11,
    padding: "12px 14px",
    cursor: "pointer",
  },
  chIco: (enabled: boolean) => ({
    width: 30,
    height: 30,
    borderRadius: 8,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 11,
    fontWeight: 700,
    color: enabled ? V.or : V.t3,
    flexShrink: 0,
    transition: "all .13s",
    background: enabled ? "rgba(249,115,22,.15)" : V.bg4,
    border: `1px solid ${enabled ? "rgba(249,115,22,.25)" : V.bd2}`,
  }),
  chName: (enabled: boolean) => ({
    flex: 1,
    fontSize: 12,
    fontWeight: 600,
    color: enabled ? V.t0 : V.t1,
  }),
  toggle: (on: boolean) => ({
    width: 32,
    height: 18,
    borderRadius: 9,
    background: on ? V.or : V.bg5,
    flexShrink: 0,
    position: "relative" as const,
    cursor: "pointer",
    transition: "background .18s",
  }),
  toggleKnob: (on: boolean) => ({
    position: "absolute" as const,
    top: 2,
    left: on ? 16 : 2,
    width: 14,
    height: 14,
    borderRadius: "50%",
    background: "#fff",
    transition: "left .18s",
  }),
  chBody: (open: boolean) => ({
    padding: open ? "0 14px 14px" : "0 14px",
    maxHeight: open ? 500 : 0,
    overflow: "hidden",
    transition: "max-height .3s ease, padding .3s ease",
  }),
  chSep: { height: 1, background: V.bd, marginBottom: 12 },
  loginTabs: {
    display: "flex",
    gap: 0,
    marginBottom: 14,
    borderBottom: `1px solid ${V.bd}`,
  },
  ltab: (active: boolean) => ({
    padding: "7px 14px",
    fontSize: 11,
    fontWeight: 500,
    color: active ? V.or : V.t3,
    cursor: "pointer",
    borderBottom: `2px solid ${active ? V.or : "transparent"}`,
    marginBottom: -1,
    transition: "all .13s",
    display: "flex",
    alignItems: "center",
    gap: 5,
    background: "none",
    border: "none",
    borderBottomWidth: 2,
    borderBottomStyle: "solid" as const,
    borderBottomColor: active ? V.or : "transparent",
    fontFamily: "inherit",
  }),
  qrArea: {
    display: "flex",
    flexDirection: "column" as const,
    alignItems: "center",
    gap: 12,
    padding: "16px 0",
  },
  qrBox: {
    width: 140,
    height: 140,
    background: V.bg4,
    border: `1px solid ${V.bd2}`,
    borderRadius: 10,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    position: "relative" as const,
    overflow: "hidden",
  },
  qrCaption: { fontSize: 11, color: V.t2, textAlign: "center" as const, lineHeight: 1.5 },
  qrStatus: {
    fontSize: 11,
    color: V.t3,
    display: "flex",
    alignItems: "center",
    gap: 6,
  },
  spinner: {
    width: 12,
    height: 12,
    border: `1.5px solid rgba(249,115,22,.2)`,
    borderTopColor: V.or,
    borderRadius: "50%",
    animation: "onb-spin .7s linear infinite",
    flexShrink: 0,
  },
  // Step 4: config
  warnBox: {
    background: V.rlo,
    border: `1px solid ${V.rbrd}`,
    borderRadius: 7,
    padding: "8px 12px",
    fontSize: 11,
    color: "#a05050",
    marginBottom: 6,
  },
  // Step 5: launch
  lc: {
    display: "flex",
    alignItems: "center",
    gap: 11,
    padding: "10px 13px",
    borderRadius: 8,
    background: V.bg3,
    border: `1px solid ${V.bd}`,
  },
  lcRes: (s: string) => ({
    fontFamily: V.mono,
    fontSize: 10,
    color: s === "ok" ? V.green : s === "loading" ? V.or : s === "error" ? V.red : V.t3,
  }),
  successBox: {
    background: V.glo,
    border: `1px solid ${V.gbrd}`,
    borderRadius: 10,
    padding: "14px 16px",
    marginBottom: 4,
  },
};

// ── Spinner CSS (injected once) ──
const SPIN_STYLE_ID = "onb-spin-style";
function ensureSpinStyle() {
  if (typeof document !== "undefined" && !document.getElementById(SPIN_STYLE_ID)) {
    const style = document.createElement("style");
    style.id = SPIN_STYLE_ID;
    style.textContent = "@keyframes onb-spin{to{transform:rotate(360deg)}}";
    document.head.appendChild(style);
  }
}

// ── Provider state ──
interface ProvState {
  selected: boolean;
  apiKey: string;
  baseUrl: string;
  apiType?: ApiType;
  userAgent: string;
  testStatus: "idle" | "testing" | "success" | "error";
  testError: string;
  models: ModelDef[] | null;
  modelsLoading: boolean;
  selectedModel: string | null;
  inputState: "" | "ok" | "err";
}

function makeProvState(selected: boolean, isUrl?: boolean, defaultUserAgent?: string): ProvState {
  return {
    selected,
    apiKey: "",
    baseUrl: isUrl ? "http://localhost:11434/v1" : "",
    apiType: undefined,
    userAgent: defaultUserAgent || "",
    testStatus: "idle",
    testError: "",
    models: null,
    modelsLoading: false,
    selectedModel: null,
    inputState: "",
  };
}

// ── Channel state ──
interface ChState {
  enabled: boolean;
  activeTab: "qr" | "cred";
  credValues: Record<string, string>;
  qrUrl: string | null;
  qrStatus: "idle" | "waiting" | "scanned" | "confirmed" | "error";
}

function makeChState(): ChState {
  return {
    enabled: false,
    activeTab: "qr",
    credValues: {},
    qrUrl: null,
    qrStatus: "idle",
  };
}

// ── Component ──

export function OnboardingPage() {
  const navigate = useNavigate();
  const [wizLang, setWizLang] = useState<WizLang>(detectWizLang);
  const [step, setStep] = useState(0); // 0 = welcome screen
  const t = T[wizLang];
  const isZh = wizLang === "cn";
  const PROVIDERS = useMemo(() => getProviders(wizLang), [wizLang]);
  const CHANNELS = useMemo(() => getChannels(wizLang), [wizLang]);

  // Commit language selection and enter step 1
  const enterWizard = () => {
    try { localStorage.setItem("rsclaw-lang", wizLang); } catch {}
    // Also set the app-level lang key so getLang() picks it up
    try { localStorage.setItem("lang", wizLang); } catch {}
    setStep(1);
  };

  // Step 1
  const [rscReady, setRscReady] = useState(false);
  const [rscVersion, setRscVersion] = useState("");
  const [openclawPath, setOpenclawPath] = useState<string | null>(null);
  const [openclawScan, setOpenclawScan] = useState<{ agents: number; sessions: number; jsonl: number } | null>(null);
  const [rscConfigPath, setRscConfigPath] = useState("");
  const [detecting, setDetecting] = useState(true);
  const [migrating, setMigrating] = useState(false);
  const [migrateResult, setMigrateResult] = useState<"idle" | "ok" | "error">("idle");

  // Step 2
  const [provs, setProvs] = useState<Record<string, ProvState>>(() => {
    const m: Record<string, ProvState> = {};
    Object.values(ALL_PROVIDERS).forEach((p) => {
      const ps = makeProvState(false, p.isUrl, p.defaultUserAgent);
      if (p.id === "custom") {
        // Leave apiType and baseUrl empty - user picks them
        ps.apiType = undefined;
        ps.baseUrl = "";
      } else if (p.hasBaseUrl && p.defaultBaseUrl) {
        // Pre-fill default URL for providers with editable URL (e.g. doubao)
        ps.baseUrl = p.defaultBaseUrl;
      }
      m[p.id] = ps;
    });
    return m;
  });
  // Select first provider when entering step 2
  const provsInitRef = useRef(false);
  useEffect(() => {
    if (step === 2 && !provsInitRef.current && PROVIDERS.length > 0) {
      provsInitRef.current = true;
      const firstId = PROVIDERS[0].id;
      setProvs((prev) => {
        const p: Record<string, ProvState> = {};
        for (const [k, v] of Object.entries(prev)) {
          p[k] = { ...v, selected: k === firstId };
        }
        return p;
      });
    }
  }, [step, PROVIDERS]);

  // Step 3
  const [chs, setChs] = useState<Record<string, ChState>>(() => {
    const m: Record<string, ChState> = {};
    Object.values(ALL_CHANNELS).forEach((c) => {
      m[c.id] = makeChState();
    });
    return m;
  });
  // Select first channel when entering step 3
  const chsInitRef = useRef(false);
  useEffect(() => {
    if (step === 3 && !chsInitRef.current && CHANNELS.length > 0) {
      chsInitRef.current = true;
      const firstId = CHANNELS[0].id;
      setChs((prev) => {
        const c: Record<string, ChState> = {};
        for (const [k, v] of Object.entries(prev)) {
          c[k] = { ...v, enabled: k === firstId };
        }
        return c;
      });
    }
  }, [step, CHANNELS]);
  const qrPollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const qrTokenRef = useRef<string | null>(null);

  // Step 4
  const [bindMode, setBindMode] = useState<"loopback" | "all" | "custom">("loopback");
  const [customIp, setCustomIp] = useState("");
  const [port, setPort] = useState("18888");
  const [language, setLanguage] = useState(LANG_TO_CONFIG[wizLang]);

  // Step 5
  const [launching, setLaunching] = useState(false);
  const [checks, setChecks] = useState<{ label: string; icon: string; status: string }[]>([]);
  const [done, setDone] = useState(false);

  useEffect(() => {
    ensureSpinStyle();
    // Read gateway URL + auth token early so API calls work
    (async () => {
      try {
        const tauriInvoke = isTauri ? tauriInvokeV2 : null;
        if (tauriInvoke) {
          const gw: any = await tauriInvoke("get_gateway_port");
          if (gw?.url) setGatewayUrl(gw.url);
          if (gw?.token) {
            setAuthToken(gw.token);
            try { localStorage.setItem("rsclaw-auth-token", gw.token); } catch {}
          }
        }
      } catch {}
    })();
  }, []);

  // Auto-detect on mount (so results are ready when user enters step 1)
  const detectRanRef = useRef(false);
  useEffect(() => {
    if (detectRanRef.current) return;
    detectRanRef.current = true;
    (async () => {
      const tauriInvoke = isTauri ? tauriInvokeV2 : null;
      if (tauriInvoke) {
        // Check if config already has gateway.language set (e.g. from `rsclaw setup` CLI)
        try {
          const raw: string = await tauriInvoke("read_config_file");
          const cfg = JSON5.parse(raw || "{}");
          const cfgLang: string | undefined = cfg?.gateway?.language;
          if (cfgLang && typeof cfgLang === "string" && cfgLang.trim()) {
            const mapped = CONFIG_TO_LANG[cfgLang.trim()];
            if (mapped) {
              setWizLang(mapped);
              setLanguage(cfgLang.trim());
              try { localStorage.setItem("rsclaw-lang", mapped); } catch {}
              setStep(1);
            }
          }
        } catch (e) { console.warn("[onboarding] config language detection failed:", e); }
        // rsclaw installed?
        try { const setupDone = await tauriInvoke("check_setup"); setRscReady(setupDone); } catch { setRscReady(false); }
        // version
        try { const ver: string = await tauriInvoke("get_version"); if (ver) setRscVersion(ver.replace(/^rsclaw\s*/i, "").trim()); } catch {}
        // config path
        try { const cp: string = await tauriInvoke("get_config_path"); if (cp) setRscConfigPath(cp); } catch {}
        // OpenClaw detection + scan
        try {
          const oc: string | null = await tauriInvoke("detect_openclaw");
          setOpenclawPath(oc || null);
          if (oc) {
            try { const scan = await tauriInvoke("scan_openclaw", { path: oc }); setOpenclawScan(scan); } catch {}
          }
        } catch {}
      } else {
        // Browser mode: try reading config from gateway API
        try {
          const res = await fetch("http://localhost:18888/api/v1/config");
          if (res.ok) {
            const cfg = await res.json();
            const cfgLang: string | undefined = cfg?.gateway?.language;
            if (cfgLang && typeof cfgLang === "string" && cfgLang.trim()) {
              const mapped = CONFIG_TO_LANG[cfgLang.trim()];
              if (mapped) {
                setWizLang(mapped);
                setLanguage(cfgLang.trim());
                try { localStorage.setItem("rsclaw-lang", mapped); } catch {}
                setStep(1);
              }
            }
          }
        } catch (e) { console.warn("[onboarding] config language detection failed:", e); }
        // Browser mode: health check only
        try { await getHealth(); setRscReady(true); } catch { setRscReady(false); }
      }
      setDetecting(false);
    })();
  }, []);

  // ── Migration ──
  const runMigrate = async () => {
    if (!openclawPath) return;
    setMigrating(true);
    setMigrateResult("idle");
    try {
      const tauriInvoke = isTauri ? tauriInvokeV2 : null;
      if (tauriInvoke) {
        await tauriInvoke("migrate_openclaw", { sourcePath: openclawPath });
      }
      setMigrateResult("ok");
      // Migration succeeded - read port from migrated config, start gateway, jump to console
      if (tauriInvoke) {
        try {
          const gw: any = await tauriInvoke("get_gateway_port");
          if (gw?.url) { setGatewayUrl(gw.url); if (gw.token) setAuthToken(gw.token); }
        } catch {}
        try { await tauriInvoke("start_gateway"); } catch {}
      }
      setTimeout(() => {
        markSetupComplete();
        // Enable auto-start after successful migration
        import("../utils/tauri").then(({ isTauri, invoke }) => {
          if (isTauri) invoke("set_auto_start", { enable: true }).catch(() => {});
        }).catch(() => {});
        // Reload to ensure auth token is picked up by all modules
        window.location.href = "/";
      }, 2000);
    } catch (e: any) {
      setMigrateResult("error");
      const msg = typeof e === "string" ? e : e?.message || JSON.stringify(e);
      toast.fromError(isZh ? "\u8FC1\u79FB\u5931\u8D25" : "Migration failed", msg);
    } finally {
      setMigrating(false);
    }
  };

  // ── Provider logic ──
  // Single-select: only one provider at a time
  const toggleProvider = (id: string) => {
    setProvs((prev) => {
      const p: Record<string, any> = {};
      for (const [k, v] of Object.entries(prev)) {
        p[k] = { ...v, selected: k === id };
      }
      return p;
    });
  };

  const setProvKey = (id: string, key: string) => {
    setProvs((prev) => {
      const p = { ...prev };
      const cur = { ...p[id] };
      cur.apiKey = key;
      cur.testStatus = "idle";
      cur.models = null;
      cur.selectedModel = null;
      cur.inputState = "";
      cur.testError = "";
      p[id] = cur;
      return p;
    });
  };

  const setProvBaseUrl = (id: string, url: string) => {
    setProvs((prev) => {
      const p = { ...prev };
      p[id] = { ...p[id], baseUrl: url, testStatus: "idle", models: null, selectedModel: null, inputState: "", testError: "" };
      return p;
    });
  };

  const setProvUserAgent = (id: string, ua: string) => {
    setProvs((prev) => {
      const p = { ...prev };
      p[id] = { ...p[id], userAgent: ua };
      return p;
    });
  };

  const setProvApiType = (id: string, apiType: ApiType) => {
    setProvs((prev) => {
      const p = { ...prev };
      p[id] = {
        ...p[id],
        apiType,
        baseUrl: "",
        testStatus: "idle",
        models: null,
        selectedModel: null,
        inputState: "",
        testError: "",
      };
      return p;
    });
  };

  const testProvider = async (id: string) => {
    const prov = provs[id];
    const isCustomLikeTest = id === "custom" || id === "codingplan";
    const provDef = PROVIDERS.find((p) => p.id === id);
    const isUrlProvider = !!provDef?.isUrl;
    const keyRequired = isCustomLikeTest
      ? API_TYPE_NEEDS_KEY[prov.apiType || "openai"]
      : !isUrlProvider;
    if (keyRequired && !prov.apiKey.trim()) {
      setProvs((prev) => {
        const p = { ...prev };
        p[id] = { ...p[id], testError: t.enterKey, inputState: "err" };
        return p;
      });
      return;
    }

    setProvs((prev) => {
      const p = { ...prev };
      p[id] = { ...p[id], testStatus: "testing", testError: "", inputState: "" };
      return p;
    });

    try {
      // Test provider API directly (Tauri) or via gateway (browser)
      const provDef = PROVIDERS.find((p) => p.id === id);
      // For custom provider, use the dedicated baseUrl field; for isUrl providers (ollama), also use baseUrl
      const baseUrl = id === "custom"
        ? (prov.baseUrl || (prov.apiType ? API_TYPE_DEFAULT_URLS[prov.apiType] : undefined))
        : ((provDef?.isUrl || provDef?.hasBaseUrl) ? prov.baseUrl : undefined);
      const tauriInvoke = isTauri ? tauriInvokeV2 : null;
      let result: any;
      let modelIds: string[] = [];
      if (tauriInvoke) {
        result = await tauriInvoke("test_provider", { provider: id, apiKey: prov.apiKey, baseUrl: baseUrl || null, apiType: id === "custom" ? (prov.apiType || null) : null });
        modelIds = result.models || [];
      } else {
        const apiTypeParam = id === "custom" ? (prov.apiType || undefined) : undefined;
        result = await testProviderKey(id, prov.apiKey, baseUrl, apiTypeParam);
        if (result.ok) {
          const modelResult = await listProviderModels(id, prov.apiKey, baseUrl, apiTypeParam);
          modelIds = modelResult.models || [];
        }
      }
      if (!result.ok) {
        throw new Error(result.error || "Test failed");
      }
      setProvs((prev) => {
        const p = { ...prev };
        p[id] = { ...p[id], testStatus: "success", inputState: "ok", modelsLoading: true };
        return p;
      });
      const realModels: ModelDef[] = modelIds.slice(0, 20).map((mid: string) => ({
        id: mid,
        tag: "",
        tagEn: "",
        rec: false,
      }));
      // If no models returned, use fallback defaults
      const fallback = MODELS[id] || [];
      const finalModels = realModels.length > 0 ? realModels : fallback;
      setProvs((prev) => {
        const p = { ...prev };
        p[id] = {
          ...p[id],
          modelsLoading: false,
          models: finalModels,
          selectedModel: finalModels[0]?.id || null,
        };
        return p;
      });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setProvs((prev) => {
        const p = { ...prev };
        p[id] = {
          ...p[id],
          testStatus: "error",
          inputState: "err",
          testError: msg.includes("Failed to fetch")
            ? (isZh ? "\u7F51\u5173\u672A\u8FD0\u884C\uFF0C\u8BF7\u5148\u542F\u52A8\u7F51\u5173" : "Gateway not running, please start it first")
            : (isZh ? "API Key \u9A8C\u8BC1\u5931\u8D25: " : "API Key validation failed: ") + msg,
        };
        return p;
      });
    }
  };

  const selectModel = (provId: string, modelId: string) => {
    setProvs((prev) => {
      const p = { ...prev };
      p[provId] = { ...p[provId], selectedModel: modelId };
      return p;
    });
  };

  const canNextStep2 = Object.values(provs).some(
    (p) => p.selected && p.testStatus === "success" && p.selectedModel,
  );

  // ── Channel logic ──
  // Single-select: only one channel at a time, auto-start QR on select
  const toggleChannel = (id: string) => {
    const wasEnabled = chs[id]?.enabled;
    setChs((prev) => {
      const c: Record<string, any> = {};
      for (const [k, v] of Object.entries(prev)) {
        c[k] = { ...v, enabled: k === id ? !v.enabled : false };
      }
      return c;
    });
    // Auto-start QR login when selecting a channel that supports it
    if (!wasEnabled) {
      const chDef = ALL_CHANNELS[id];
      if (chDef?.hasQr) {
        setTimeout(() => startChannelQr(id), 300);
      }
    }
  };

  const setChTab = (chId: string, tab: "qr" | "cred") => {
    setChs((prev) => {
      const c = { ...prev };
      c[chId] = { ...c[chId], activeTab: tab };
      return c;
    });
  };

  const setChCred = (chId: string, field: string, value: string) => {
    setChs((prev) => {
      const c = { ...prev };
      const cur = { ...c[chId] };
      cur.credValues = { ...cur.credValues, [field]: value };
      c[chId] = cur;
      return c;
    });
  };

  const startChannelQr = async (channelId: string) => {
    const tauriInvoke = isTauri ? tauriInvokeV2 : null;
    if (!tauriInvoke) return;

    setChs((prev) => {
      const c = { ...prev };
      c[channelId] = { ...c[channelId], qrStatus: "waiting", qrUrl: null };
      return c;
    });

    try {
      // Start login process in background (spawns rsclaw channels login <channel>)
      await tauriInvoke("channel_login_start", { channel: channelId });

      // Poll for QR image + login completion
      if (qrPollRef.current) clearInterval(qrPollRef.current);
      let attempts = 0;
      let qrFound = false;
      qrPollRef.current = setInterval(async () => {
        attempts++;
        try {
          // Check login status
          const status: string = await tauriInvoke("channel_login_status");
          if (status === "done") {
            if (qrPollRef.current) clearInterval(qrPollRef.current);
            // Read credentials written by sidecar into state
            let loginCreds: Record<string, string> = {};
            try {
              const raw: string = await tauriInvoke("read_config_file");
              const cfg = JSON5.parse(raw || "{}");
              const chCfg = cfg?.channels?.[channelId] || {};
              loginCreds = { ...chCfg };
            } catch {}
            setChs((prev) => {
              const c = { ...prev };
              c[channelId] = { ...c[channelId], qrStatus: "confirmed", credValues: { ...c[channelId]?.credValues, ...loginCreds } };
              return c;
            });
            return;
          }
          // Check for QR image
          if (!qrFound) {
            const dataUri: string | null = await tauriInvoke("channel_login_qr");
            if (dataUri) {
              qrFound = true;
              setChs((prev) => {
                const c = { ...prev };
                c[channelId] = { ...c[channelId], qrUrl: dataUri, qrStatus: "waiting" };
                return c;
              });
            }
          }
        } catch {}
        if (attempts > 60) {
          if (qrPollRef.current) clearInterval(qrPollRef.current);
        }
      }, 2000);
    } catch {
      setChs((prev) => {
        const c = { ...prev };
        c[channelId] = { ...c[channelId], qrStatus: "error" };
        return c;
      });
    }
  };

  useEffect(() => {
    return () => {
      if (qrPollRef.current) clearInterval(qrPollRef.current);
    };
  }, []);

  // ── Config generation ──
  const generateConfig = () => {
    const providers: Record<string, any> = {};
    for (const [id, ps] of Object.entries(provs)) {
      if (!ps.selected || !ps.selectedModel) continue;
      const isCustomLike = id === "custom" || id === "codingplan";
      if (isCustomLike) {
        const apiType = ps.apiType || "openai";
        const entry: Record<string, any> = { api: apiType };
        if (ps.baseUrl) entry.baseUrl = ps.baseUrl;
        if (ps.apiKey) entry.apiKey = ps.apiKey;
        if (ps.userAgent) entry.userAgent = ps.userAgent;
        providers[id] = entry;
      } else if (PROVIDERS.find((p) => p.id === id)?.isUrl) {
        providers[id] = { api: "ollama", baseUrl: ps.baseUrl || ps.apiKey };
      } else if (ps.apiKey) {
        const entry: Record<string, any> = { apiKey: ps.apiKey };
        if (ps.baseUrl) entry.baseUrl = ps.baseUrl;
        if (ps.userAgent) entry.userAgent = ps.userAgent;
        providers[id] = entry;
      } else {
        providers[id] = {};
      }
    }
    const channels: Record<string, any> = {};
    for (const [id, cs] of Object.entries(chs)) {
      if (!cs.enabled) continue;
      channels[id] = { ...cs.credValues };
    }

    const bindAddr = bindMode === "custom" ? customIp || "loopback" : bindMode;

    const defaultModel = Object.entries(provs).find(
      ([, ps]) => ps.selected && ps.selectedModel,
    );

    return JSON.stringify(
      {
        gateway: {
          bind: bindAddr,
          port: parseInt(port) || 18888,
          language,
        },
        models: { providers },
        channels,
        agents: {
          defaults: {
            model: {
              primary: defaultModel
                ? `${defaultModel[0]}/${defaultModel[1].selectedModel}`
                : "anthropic/claude-sonnet-4-20250514",
            },
          },
          list: [{ id: "main", default: true }],
        },
      },
      null,
      2,
    );
  };

  // ── Launch ──
  const runLaunch = async () => {
    setLaunching(true);
    const steps = [
      { label: t.writeConfig, icon: "\uD83D\uDCDD", status: "wait" },
      { label: t.startGateway, icon: "\u26A1", status: "wait" },
      { label: t.healthCheck, icon: "\u2764\uFE0F", status: "wait" },
      { label: t.channelVerify, icon: "\uD83D\uDD0C", status: "wait" },
      { label: t.modelTest, icon: "\uD83E\uDDE0", status: "wait" },
    ];
    setChecks([...steps]);

    const update = (idx: number, status: string) => {
      steps[idx].status = status;
      setChecks([...steps]);
    };

    try {
      // Set gateway URL to match the user's configured port
      const userPort = parseInt(port) || 18888;
      setGatewayUrl(`http://localhost:${userPort}`);

      // 1: write config — deep merge into existing, never delete existing keys
      update(0, "loading");
      const newConfig = JSON.parse(generateConfig());
      const tauriInvoke = isTauri ? tauriInvokeV2 : null;

      // Deep merge helper: recursively merge src into dst without deleting dst keys
      const deepMerge = (dst: any, src: any): any => {
        if (!src || typeof src !== "object" || Array.isArray(src)) return src;
        const result = { ...(dst || {}) };
        for (const [k, v] of Object.entries(src)) {
          if (v && typeof v === "object" && !Array.isArray(v) && typeof result[k] === "object" && !Array.isArray(result[k])) {
            result[k] = deepMerge(result[k], v);
          } else {
            result[k] = v;
          }
        }
        return result;
      };

      if (tauriInvoke) {
        try { await tauriInvoke("run_setup"); } catch {} // ensure dirs exist
        // Read existing config — this is the source of truth
        let existing: any = {};
        try {
          const raw: string = await tauriInvoke("read_config_file");
          existing = JSON5.parse(raw || "{}");
        } catch {}
        // Deep merge: existing config is base, new config overlays on top
        const merged = deepMerge(existing, newConfig);
        await tauriInvoke("write_config", { content: JSON.stringify(merged, null, 2) });
      } else {
        // Non-Tauri: fetch existing config, JSON5-parse, deep merge, save.
        let existing: any = {};
        try {
          const data = await getConfig();
          existing = JSON5.parse(data.raw || "{}");
        } catch {}
        const merged = deepMerge(existing, newConfig);
        await saveConfig({ raw: JSON.stringify(merged, null, 2) });
      }
      // Re-read gateway URL and auth token from the merged config
      if (tauriInvoke) {
        try {
          const gw: any = await tauriInvoke("get_gateway_port");
          if (gw?.url) {
            setGatewayUrl(gw.url);
            if (gw.token) {
              setAuthToken(gw.token);
              try { localStorage.setItem("rsclaw-auth-token", gw.token); } catch {}
            }
          }
        } catch {}
      }
      update(0, "ok");

      // 2: start gateway (stop any existing one first)
      update(1, "loading");
      if (tauriInvoke) {
        try { await tauriInvoke("stop_gateway"); } catch {}
        await new Promise((r) => setTimeout(r, 1000));
        await tauriInvoke("start_gateway");
      }
      await new Promise((r) => setTimeout(r, 2500));
      update(1, "ok");

      // 3: health check
      update(2, "loading");
      try {
        await getHealth();
        update(2, "ok");
      } catch {
        update(2, "warn");
      }

      // 4: channel verify
      update(3, "loading");
      await new Promise((r) => setTimeout(r, 800));
      update(3, "ok");

      // 5: model test
      update(4, "loading");
      await new Promise((r) => setTimeout(r, 800));
      update(4, "ok");

      setDone(true);
    } catch (e) {
      const idx = steps.findIndex((s) => s.status === "loading");
      if (idx >= 0) update(idx, "error");
      toast.fromError(isZh ? "\u542F\u52A8\u5931\u8D25" : "Launch failed", e);
    } finally {
      setLaunching(false);
    }
  };

  const enableAutoStart = () => {
    import("../utils/tauri").then(({ isTauri, invoke }) => {
      if (isTauri) invoke("set_auto_start", { enable: true }).catch(() => {});
    }).catch(() => {});
  };

  const finish = () => {
    markSetupComplete();
    enableAutoStart();
    window.location.href = "/";
  };

  const skipAll = () => {
    markSetupComplete();
    enableAutoStart();
    window.location.href = "/#/rsclaw-panel";
  };

  const stepState = (n: number) => (n < step ? "done" : n === step ? "active" : "todo");
  const stepLabels = t.stepLabels;
  const progPct = step === 0 ? 0 : [20, 40, 60, 80, 100][step - 1];

  // ── Render helpers ──

  const renderSpinner = (color?: string) => (
    <div
      style={{
        ...S.spinner,
        ...(color === "green"
          ? { border: `1.5px solid rgba(45,212,160,.2)`, borderTopColor: V.green }
          : {}),
      }}
    />
  );

  const renderQrPixels = () => {
    const pixels: string[] = [];
    const on = V.t0;
    const off = V.bg3;
    for (let i = 0; i < 121; i++) pixels.push(Math.random() > 0.45 ? on : off);
    const corners = [
      0, 1, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16, 17, 18, 22, 24, 25, 29, 33, 35, 36,
      37, 38, 39, 40, 41, 42, 43, 77, 78, 79, 80, 81, 82, 83, 88, 90, 91, 95, 99, 101,
      102, 103, 104, 105, 106, 107, 108, 113, 115, 116, 117, 118, 119, 120,
    ];
    corners.forEach((i) => (pixels[i] = on));
    return (
      <div
        style={{
          width: 110,
          height: 110,
          display: "grid",
          gridTemplateColumns: "repeat(11, 1fr)",
          gap: 1,
        }}
      >
        {pixels.map((c, i) => (
          <div key={i} style={{ background: c, borderRadius: 1 }} />
        ))}
      </div>
    );
  };

  return (
    <div style={S.page}>
      {/* Drag region for Tauri title bar overlay */}
      <div data-tauri-drag-region style={{ position: "fixed", top: 0, left: 0, right: 0, height: 32, zIndex: 201, WebkitAppRegion: "drag" } as any} />
      <div style={S.window}>
      {/* Progress bar - hidden on welcome screen */}
      {step > 0 && (
        <div style={{ width: "100%", height: 2, background: V.bg3, borderRadius: 1, overflow: "hidden" }}>
          <div style={S.pfill(progPct)} />
        </div>
      )}

        {/* Wizard body */}
        <div style={S.wiz}>
          {/* Logo */}
          <div style={S.logoWrap}>
            <div style={S.logoBox}>
              <img
                src="/rsclaw-icon.svg"
                alt=""
                style={{ width: 32, height: 32, borderRadius: 8 }}
              />
            </div>
            <div style={S.logoName}>
              <span style={{ color: V.or }}>Rs</span>Claw
            </div>
            {step === 0 ? (
              <div style={{ fontSize: 11, color: V.t2, textAlign: "center", lineHeight: 1.6, maxWidth: 380 }}>
                {"\u8783\u87F9AI\u81EA\u52A8\u5316\u7BA1\u5BB6 \u00B7 Your AI Automation Manager"}
              </div>
            ) : (
              <div style={S.logoSub}>
                {t.subtitle}
              </div>
            )}
          </div>

          {/* Welcome screen (step 0) */}
          {step === 0 && (
            <div style={{ width: "100%", display: "flex", flexDirection: "column", alignItems: "center" }}>
              <div style={{
                display: "grid",
                gridTemplateColumns: "1fr 1fr",
                gap: 10,
                width: "100%",
                maxWidth: 380,
                marginBottom: 28,
              }}>
                {LANG_GRID.map((lg) => (
                  <div
                    key={lg.code}
                    onClick={() => setWizLang(lg.code)}
                    style={{
                      padding: "10px 14px",
                      borderRadius: 9,
                      cursor: "pointer",
                      textAlign: "center",
                      fontSize: 13,
                      fontWeight: 500,
                      color: wizLang === lg.code ? V.or : V.t1,
                      background: wizLang === lg.code ? V.olo : V.bg3,
                      border: `1.5px solid ${wizLang === lg.code ? V.or : V.bd2}`,
                      transition: "all 0.15s",
                    }}
                  >
                    {lg.label}
                  </div>
                ))}
              </div>
              <button
                style={{ ...S.btnNext, padding: "10px 32px", fontSize: 13 }}
                onClick={enterWizard}
              >
                {t.welcome} {"\u2192"}
              </button>
            </div>
          )}

          {/* Steps indicator - hidden on welcome screen */}
          {step > 0 && (
            <div style={S.steps}>
              {[1, 2, 3, 4, 5].map((n) => (
                <div key={n} style={S.stp(n === 5)}>
                  <div style={S.stepUnit}>
                    <div style={S.sc(stepState(n))}>
                      {n < step ? "\u2713" : n}
                    </div>
                    <div style={S.slbl(stepState(n))}>{stepLabels[n - 1]}</div>
                  </div>
                  {n < 5 && <div style={S.sl(n < step)} />}
                </div>
              ))}
            </div>
          )}

          {/* ── STEP 1: Detect ── */}
          {step === 1 && (
            <div style={S.card}>
              <div style={S.cardTitle}>{t.step1Title}</div>
              <div style={S.cardSub}>{t.step1Sub}</div>

              <div style={S.detRow}>
                <div style={{ fontSize: 18, flexShrink: 0, width: 22, textAlign: "center" as const }}>
                  {"\uD83D\uDCE6"}
                </div>
                <div style={{ flex: 1 }}>
                  <div style={{ fontSize: 12, fontWeight: 500, color: V.t0 }}>rsclaw</div>
                  <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>
                    {rscConfigPath || "~/.rsclaw/"}{rscVersion ? ` \u00B7 ${rscVersion}` : ""}
                  </div>
                </div>
                {detecting ? (
                  renderSpinner()
                ) : rscReady ? (
                  <span style={S.pillOk}>{t.found}</span>
                ) : (
                  <span style={S.pillWarn}>{t.notFound}</span>
                )}
              </div>

              <div style={S.detRow}>
                <div style={{ fontSize: 18, flexShrink: 0, width: 22, textAlign: "center" as const }}>
                  {"\uD83D\uDD04"}
                </div>
                <div style={{ flex: 1 }}>
                  <div style={{ fontSize: 12, fontWeight: 500, color: V.t0 }}>OpenClaw</div>
                  <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>
                    {openclawPath || "~/.openclaw/"}
                  </div>
                  {openclawScan && openclawScan.agents > 0 && (
                    <div style={{ fontSize: 10, color: V.t2, fontFamily: V.mono, marginTop: 3 }}>
                      {openclawScan.agents} agents {"\u00B7"} {openclawScan.sessions} sessions {"\u00B7"} {openclawScan.jsonl} jsonl
                    </div>
                  )}
                </div>
                {detecting ? (
                  renderSpinner()
                ) : openclawPath ? (
                  <span style={S.pillWarn}>{t.migratable}</span>
                ) : (
                  <span style={S.pillSkip}>{t.skipLabel}</span>
                )}
              </div>

              {/* When OpenClaw detected AND no rsclaw config yet: two-choice buttons */}
              {!detecting && openclawPath && !rscReady && migrateResult === "idle" && !migrating && (
                <div style={{ marginTop: 8 }}>
                  <div style={S.infoNote}>
                    <span style={{ fontSize: 13, flexShrink: 0 }}>{"\u2139"}</span>
                    <span>{t.envNoteMigrate}</span>
                  </div>
                  <div style={{ display: "flex", gap: 10, marginTop: 12 }}>
                    <button style={{ ...S.btnNext, flex: 1, padding: "10px 0", fontSize: 13 }} onClick={runMigrate}>
                      {isZh ? "\u8FC1\u79FB\u5B89\u88C5" : "Migrate & Install"}
                    </button>
                    <button style={{ ...S.btnPrev, flex: 1, padding: "10px 0", fontSize: 13 }} onClick={() => setStep(2)}>
                      {isZh ? "\u5168\u65B0\u5B89\u88C5" : "New Install"}
                    </button>
                  </div>
                </div>
              )}

              {/* Migrating spinner */}
              {migrating && (
                <div style={{ display: "flex", alignItems: "center", justifyContent: "center", gap: 8, padding: "16px 0", marginTop: 8 }}>
                  {renderSpinner()} <span style={{ fontSize: 12, color: V.or }}>{t.migrating}</span>
                </div>
              )}

              {/* Migration success */}
              {migrateResult === "ok" && (
                <div style={{ background: V.glo, border: `1px solid ${V.gbrd}`, borderRadius: 9, padding: "12px 14px", marginTop: 8 }}>
                  <div style={{ fontSize: 12, fontWeight: 600, color: V.green, display: "flex", alignItems: "center", gap: 6 }}>
                    {"\u2713"} {t.migrateOk}
                  </div>
                </div>
              )}

              {/* Migration error */}
              {migrateResult === "error" && (
                <div style={{ background: V.rlo, border: `1px solid ${V.rbrd}`, borderRadius: 9, padding: "12px 14px", marginTop: 8 }}>
                  <div style={{ fontSize: 11, color: V.red, marginBottom: 8 }}>
                    {isZh ? "\u8FC1\u79FB\u5931\u8D25" : "Migration failed"}
                  </div>
                  <button style={{ ...S.btnPrev, fontSize: 11, padding: "6px 14px" }} onClick={() => setMigrateResult("idle")}>
                    {isZh ? "\u91CD\u8BD5" : "Retry"}
                  </button>
                </div>
              )}

              {/* Normal flow: no OpenClaw, or rsclaw config already exists */}
              {!detecting && (!openclawPath || rscReady) && migrateResult === "idle" && (
                <>
                  <div style={S.infoNote}>
                    <span style={{ fontSize: 13, flexShrink: 0 }}>{"\u2139"}</span>
                    <span>{openclawPath && rscReady
                      ? (isZh ? "\u5F53\u524D\u73AF\u5883\u5DF2\u5B58\u5728 rsclaw \u6570\u636E\uFF0C\u6682\u4E0D\u652F\u6301\u8FC1\u79FB\uFF01" : "RsClaw data already exists, migration not supported.")
                      : t.envNote}</span>
                  </div>
                  <div style={S.navRow}>
                    <button style={S.skipLink} onClick={skipAll}>{t.skip}</button>
                    <button style={S.btnNext} onClick={() => setStep(2)}>{t.next}</button>
                  </div>
                </>
              )}
            </div>
          )}

          {/* ── STEP 2: Providers ── */}
          {step === 2 && (
            <div style={S.card}>
              <div style={S.cardTitle}>{t.step2Title}</div>
              <div style={S.cardSub}>{t.step2Sub}</div>

              {/* List selector */}
              <div style={{ border: "1px solid rgba(255,255,255,0.055)", borderRadius: 10, maxHeight: 155, overflowY: "auto", background: "#141618", marginBottom: 0 }}>
                {PROVIDERS.map((pDef) => {
                  if (pDef.sep) return <div key={pDef.id} style={{ borderBottom: "1px solid rgba(255,255,255,0.055)" }} />;
                  const ps = provs[pDef.id];
                  const selected = ps?.selected;
                  return (
                    <div
                      key={pDef.id}
                      onClick={() => toggleProvider(pDef.id)}
                      style={{
                        display: "flex", alignItems: "center", gap: 10,
                        padding: "9px 14px", cursor: "pointer",
                        borderBottom: "1px solid rgba(255,255,255,0.055)",
                        background: selected ? "transparent" : "transparent",
                        transition: "background 0.1s",
                      }}
                      onMouseEnter={(e) => (e.currentTarget.style.background = "#1f2126")}
                      onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
                    >
                      <div style={{
                        width: 16, height: 16, borderRadius: "50%", flexShrink: 0,
                        display: "flex", alignItems: "center", justifyContent: "center",
                        border: selected ? "none" : "1.5px solid #252830",
                        background: selected ? "#f97316" : "transparent",
                        transition: "all 0.13s",
                      }}>
                        {selected && <div style={{ width: 6, height: 6, borderRadius: "50%", background: "#fff" }} />}
                      </div>
                      <span style={{ fontSize: 12, fontWeight: 500, color: selected ? "#eceaf4" : "#9896a4", flex: 1, fontFamily: "'JetBrains Mono', monospace", transition: "color 0.1s" }}>
                        {pDef.name}
                      </span>
                      <span style={{ fontSize: 10, color: selected ? "rgba(249,115,22,0.6)" : "#2e2c3a", whiteSpace: "nowrap" }}>
                        {isZh ? pDef.tag : pDef.tagEn}
                      </span>
                    </div>
                  );
                })}
              </div>

              {/* Expand area: key + models for active provider */}
              {(() => {
                const activeId = Object.entries(provs).find(([_, v]) => v.selected)?.[0];
                if (!activeId) return null;
                const pDef = PROVIDERS.find((p) => p.id === activeId);
                if (!pDef || pDef.sep) return null;
                const ps = provs[activeId];
                const isCustomLike = activeId === "custom" || activeId === "codingplan";
                const curApiType: ApiType = ps.apiType || "openai";
                const keyRequired = !isCustomLike || API_TYPE_NEEDS_KEY[curApiType];
                const inputFieldStyle = { flex: 1, background: "#1f2126", border: `1px solid ${ps.inputState === "ok" ? "#2dd4a0" : ps.inputState === "err" ? "#d95f5f" : "rgba(255,255,255,0.09)"}`, borderRadius: 7, padding: "7px 10px", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none" } as const;
                const fieldLabelStyle = { fontSize: 10, color: "#2e2c3a", letterSpacing: 0.4, marginBottom: 5, fontFamily: "'JetBrains Mono', monospace" } as const;
                const plainInputStyle = { width: "100%", background: "#1f2126", border: "1px solid rgba(255,255,255,0.09)", borderRadius: 7, padding: "7px 10px", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none", boxSizing: "border-box" } as const;
                const selectStyle = { width: "100%", background: "#1f2126", border: "1px solid rgba(255,255,255,0.09)", borderRadius: 7, padding: "7px 10px", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none", cursor: "pointer" } as const;
                return (
                  <div style={{ marginTop: 12, background: "#1a1c22", border: "1px solid rgba(255,255,255,0.055)", borderRadius: 10, padding: 16 }}>
                    {/* Custom/CodingPlan: api_type dropdown */}
                    {isCustomLike && (
                      <div style={{ marginBottom: 10 }}>
                        <div style={fieldLabelStyle}>API Type</div>
                        <select
                          style={selectStyle}
                          value={curApiType}
                          onChange={(e) => setProvApiType(activeId, e.target.value as ApiType)}
                        >
                          {(Object.keys(API_TYPE_LABELS) as ApiType[]).map((at) => (
                            <option key={at} value={at}>{API_TYPE_LABELS[at]}</option>
                          ))}
                        </select>
                      </div>
                    )}
                    {/* Custom/CodingPlan: Base URL input */}
                    {isCustomLike && (
                      <div style={{ marginBottom: 10 }}>
                        <div style={fieldLabelStyle}>Base URL</div>
                        <input
                          type="text"
                          style={plainInputStyle}
                          value={ps.baseUrl}
                          onChange={(e) => setProvBaseUrl(activeId, e.target.value)}
                          placeholder="https://your-api-server.com/v1"
                        />
                      </div>
                    )}
                    {/* Standard (non-custom) providers: show their key label and input inline with test button */}
                    {!isCustomLike && (
                      <>
                        <div style={fieldLabelStyle}>{pDef.keyLabel}</div>
                        <div style={{ display: "flex", gap: 8, marginBottom: 10 }}>
                          <input
                            type={pDef.isUrl ? "text" : "password"}
                            style={inputFieldStyle}
                            value={pDef.isUrl ? (ps.baseUrl || ps.apiKey) : ps.apiKey}
                            onChange={(e) => pDef.isUrl ? setProvBaseUrl(activeId, e.target.value) : setProvKey(activeId, e.target.value)}
                            placeholder={pDef.keyPlaceholder}
                          />
                          <button
                            onClick={() => testProvider(activeId)}
                            disabled={ps.testStatus === "testing"}
                            style={{ padding: "7px 13px", borderRadius: 7, border: `1px solid ${ps.testStatus === "success" ? "rgba(45,212,160,0.18)" : "rgba(255,255,255,0.09)"}`, background: ps.testStatus === "success" ? "rgba(45,212,160,0.07)" : "#1f2126", color: ps.testStatus === "success" ? "#2dd4a0" : "#9896a4", fontSize: 11, fontWeight: 500, cursor: "pointer", whiteSpace: "nowrap", flexShrink: 0, fontFamily: "inherit" }}
                          >
                            {ps.testStatus === "testing" ? (<>{renderSpinner()}{t.testing}</>)
                              : ps.testStatus === "success" ? t.connected
                              : t.test}
                          </button>
                        </div>
                        {pDef.hasBaseUrl && (
                          <div style={{ marginBottom: 10 }}>
                            <div style={fieldLabelStyle}>API URL</div>
                            <input
                              type="text"
                              style={plainInputStyle}
                              value={ps.baseUrl}
                              onChange={(e) => setProvBaseUrl(activeId, e.target.value)}
                              placeholder={pDef.defaultBaseUrl || "https://..."}
                            />
                          </div>
                        )}
                        {pDef.defaultUserAgent !== undefined && (
                          <div style={{ marginBottom: 10 }}>
                            <div style={fieldLabelStyle}>User-Agent</div>
                            <input
                              type="text"
                              style={plainInputStyle}
                              value={ps.userAgent}
                              onChange={(e) => setProvUserAgent(activeId, e.target.value)}
                              placeholder={pDef.defaultUserAgent || "Mozilla/5.0 (compatible; rsclaw/1.0)"}
                            />
                          </div>
                        )}
                      </>
                    )}
                    {/* Custom/CodingPlan: API Key + test button row */}
                    {isCustomLike && (
                      <div style={{ marginBottom: 10 }}>
                        <div style={fieldLabelStyle}>API Key{!keyRequired && <span style={{ color: "#666", fontWeight: 400 }}> (optional)</span>}</div>
                        <div style={{ display: "flex", gap: 8, marginBottom: 0 }}>
                          <input
                            type="password"
                            style={inputFieldStyle}
                            value={ps.apiKey}
                            onChange={(e) => setProvKey(activeId, e.target.value)}
                            placeholder={keyRequired ? "sk-..." : "(optional)"}
                          />
                          <button
                            onClick={() => testProvider(activeId)}
                            disabled={ps.testStatus === "testing"}
                            style={{ padding: "7px 13px", borderRadius: 7, border: `1px solid ${ps.testStatus === "success" ? "rgba(45,212,160,0.18)" : "rgba(255,255,255,0.09)"}`, background: ps.testStatus === "success" ? "rgba(45,212,160,0.07)" : "#1f2126", color: ps.testStatus === "success" ? "#2dd4a0" : "#9896a4", fontSize: 11, fontWeight: 500, cursor: "pointer", whiteSpace: "nowrap", flexShrink: 0, fontFamily: "inherit" }}
                          >
                            {ps.testStatus === "testing" ? (<>{renderSpinner()}{t.testing}</>)
                              : ps.testStatus === "success" ? t.connected
                              : t.test}
                          </button>
                        </div>
                      </div>
                    )}
                    {/* Custom/CodingPlan: User-Agent field */}
                    {isCustomLike && (
                      <div style={{ marginBottom: 10 }}>
                        <div style={fieldLabelStyle}>User-Agent <span style={{ color: "#666", fontWeight: 400 }}>(optional)</span></div>
                        <input
                          type="text"
                          style={plainInputStyle}
                          value={ps.userAgent}
                          onChange={(e) => setProvUserAgent(activeId, e.target.value)}
                          placeholder="e.g. claude-code/0.1.0"
                        />
                      </div>
                    )}
                    {ps.testError && <div style={{ fontSize: 11, color: "#d95f5f", marginBottom: 8 }}>{ps.testError}</div>}
                    {ps.testStatus === "success" && (ps.models?.length || 0) > 0 && (
                      <>
                        <div style={{ fontSize: 10, color: "#2e2c3a", marginBottom: 8 }}>{t.selectModel} <span style={{ color: "#2e2c3a", fontSize: 9 }}>{(ps.models?.length || 0)}</span></div>
                        <div style={{ display: "flex", flexDirection: "column", gap: 4, maxHeight: 140, overflowY: "auto" }}>
                          {ps.models?.map((m) => (
                            <div key={m.id} onClick={() => selectModel(activeId, m.id)}
                              style={{ display: "flex", alignItems: "center", gap: 9, padding: "7px 10px", borderRadius: 7, cursor: "pointer", border: `1px solid ${ps.selectedModel === m.id ? "rgba(249,115,22,0.2)" : "transparent"}`, background: ps.selectedModel === m.id ? "rgba(249,115,22,0.09)" : "transparent", transition: "all 0.12s", flexShrink: 0 }}>
                              <div style={{ width: 13, height: 13, borderRadius: "50%", border: `1.5px solid ${ps.selectedModel === m.id ? "#f97316" : "#252830"}`, background: ps.selectedModel === m.id ? "#f97316" : "transparent", display: "flex", alignItems: "center", justifyContent: "center", flexShrink: 0, transition: "all 0.13s" }}>
                                {ps.selectedModel === m.id && <div style={{ width: 5, height: 5, borderRadius: "50%", background: "#fff" }} />}
                              </div>
                              <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 11, flex: 1, color: ps.selectedModel === m.id ? "#eceaf4" : "#9896a4" }}>{m.id}</span>
                              {m.tag && <span style={{ fontSize: 9, padding: "1px 6px", borderRadius: 3, fontWeight: 500, background: m.rec ? "rgba(249,115,22,0.09)" : "#1f2126", color: m.rec ? "#f97316" : "#4a4858" }}>{isZh ? m.tag : m.tagEn}</span>}
                            </div>
                          ))}
                        </div>
                      </>
                    )}
                  </div>
                );
              })()}

              <div style={S.navRow}>
                <button style={S.btnPrev} onClick={() => setStep(1)}>{t.prev}</button>
                <button
                  style={canNextStep2 ? S.btnNext : S.btnNextDisabled}
                  onClick={async () => {
                    if (!canNextStep2) return;
                    // Ensure rsclaw dirs + config exist before channel login
                    try {
                      const invoke = isTauri ? tauriInvokeV2 : null;
                      if (invoke) await invoke("run_setup");
                    } catch {}
                    setStep(3);
                  }}
                  disabled={!canNextStep2}
                >{t.next}</button>
              </div>
            </div>
          )}

          {/* ── STEP 3: Channels ── */}
          {step === 3 && (
            <div style={S.card}>
              <div style={S.cardTitle}>{t.step3Title}</div>
              <div style={S.cardSub}>{t.step3Sub}</div>

              {/* Channel list - same style as provider list */}
              <div style={{ border: `1px solid ${V.bd}`, borderRadius: 10, maxHeight: 155, overflowY: "auto", background: V.bg2, marginBottom: 0 }}>
                {CHANNELS.map((chDef) => {
                  const cs = chs[chDef.id];
                  const enabled = cs?.enabled;
                  return (
                    <div
                      key={chDef.id}
                      onClick={() => toggleChannel(chDef.id)}
                      style={{
                        display: "flex", alignItems: "center", gap: 10,
                        padding: "9px 14px", cursor: "pointer",
                        borderBottom: `1px solid ${V.bd}`,
                        transition: "background 0.1s",
                      }}
                      onMouseEnter={(e) => (e.currentTarget.style.background = V.bg4)}
                      onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
                    >
                      <div style={{
                        width: 16, height: 16, borderRadius: "50%", flexShrink: 0,
                        display: "flex", alignItems: "center", justifyContent: "center",
                        border: enabled ? "none" : `1.5px solid ${V.bg5}`,
                        background: enabled ? V.green : "transparent",
                        transition: "all 0.13s",
                      }}>
                        {enabled && <div style={{ width: 6, height: 6, borderRadius: "50%", background: "#fff" }} />}
                      </div>
                      <span style={{ fontSize: 12, fontWeight: 500, color: enabled ? V.t0 : V.t1, flex: 1, fontFamily: V.mono, transition: "color 0.1s" }}>
                        {isZh ? chDef.name : chDef.nameEn}
                      </span>
                      <span style={{ fontSize: 10, color: enabled ? `rgba(45,212,160,0.5)` : V.t3, whiteSpace: "nowrap" }}>
                        {chDef.hasQr ? (isZh ? "\u626B\u7801/\u51ED\u8BC1" : "QR/Cred") : (isZh ? "\u51ED\u8BC1" : "Cred")}
                      </span>
                    </div>
                  );
                })}
              </div>

              {/* Channel credential expand area - show all enabled channels */}
              {Object.entries(chs).filter(([_, v]) => v.enabled).map(([activeId]) => {
                const chDef = CHANNELS.find((c) => c.id === activeId);
                if (!chDef) return null;

                // Per-channel credential fields
                const CRED_FIELDS: Record<string, { key: string; label: string; type: string; ph: string }[]> = {
                  wechat:   [{ key: "botId", label: "Bot ID", type: "text", ph: "xxx@im.bot" }, { key: "botToken", label: "Bot Token", type: "password", ph: "${WECHAT_BOT_TOKEN}" }],
                  feishu:   [{ key: "appId", label: "App ID", type: "text", ph: "cli_xxx" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${FEISHU_APP_SECRET}" }],
                  wecom:    [{ key: "botId", label: "Bot ID", type: "text", ph: "" }, { key: "secret", label: "Secret", type: "password", ph: "${WECOM_SECRET}" }],
                  dingtalk: [{ key: "appKey", label: "App Key", type: "text", ph: "" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${DINGTALK_APP_SECRET}" }],
                  telegram: [{ key: "botToken", label: "Bot Token", type: "password", ph: "${TELEGRAM_BOT_TOKEN}" }],
                  discord:  [{ key: "token", label: "Bot Token", type: "password", ph: "${DISCORD_BOT_TOKEN}" }],
                  slack:    [{ key: "botToken", label: "Bot Token", type: "password", ph: "${SLACK_BOT_TOKEN}" }, { key: "appToken", label: "App Token", type: "password", ph: "${SLACK_APP_TOKEN}" }],
                  whatsapp: [{ key: "phoneNumberId", label: "Phone Number ID", type: "text", ph: "" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${WHATSAPP_TOKEN}" }],
                  qq:       [{ key: "appId", label: "App ID", type: "text", ph: "" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${QQ_APP_SECRET}" }],
                  line:     [{ key: "channelSecret", label: "Channel Secret", type: "password", ph: "${LINE_CHANNEL_SECRET}" }, { key: "channelAccessToken", label: "Access Token", type: "password", ph: "${LINE_ACCESS_TOKEN}" }],
                  zalo:     [{ key: "appId", label: "App ID", type: "text", ph: "" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${ZALO_ACCESS_TOKEN}" }],
                  matrix:   [{ key: "homeserver", label: "Homeserver", type: "text", ph: "https://matrix.org" }, { key: "userId", label: "User ID", type: "text", ph: "@bot:matrix.org" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${MATRIX_ACCESS_TOKEN}" }],
                  signal:   [{ key: "phoneNumber", label: "Phone Number", type: "text", ph: "+1234567890" }],
                };

                const fields = CRED_FIELDS[activeId] || [];
                const cs = chs[activeId];
                const hasQr = chDef.hasQr;

                return (
                  <div key={activeId} style={{ marginTop: 12, background: V.bg3, border: `1px solid ${V.bd}`, borderRadius: 10, padding: 16, maxHeight: 320, overflowY: "auto" }}>
                    {/* QR / Credential tabs for wechat/feishu */}
                    {hasQr && (
                      <div style={{ display: "flex", gap: 0, borderBottom: `1px solid ${V.bd}`, marginBottom: 12, marginLeft: -16, marginRight: -16, paddingLeft: 16, paddingRight: 16 }}>
                        <button
                          onClick={() => setChTab(activeId, "qr")}
                          style={{ padding: "6px 12px", fontSize: 11, fontWeight: 500, color: cs.activeTab === "qr" ? V.or : V.t3, cursor: "pointer", borderBottom: cs.activeTab === "qr" ? `2px solid ${V.or}` : "2px solid transparent", marginBottom: -1, background: "none", border: "none", borderLeft: "none", borderRight: "none", borderTop: "none", fontFamily: "inherit" }}
                        >
                          {"\uD83D\uDCF1"} {t.qrScan}
                        </button>
                        <button
                          onClick={() => setChTab(activeId, "cred")}
                          style={{ padding: "6px 12px", fontSize: 11, fontWeight: 500, color: cs.activeTab === "cred" ? V.or : V.t3, cursor: "pointer", borderBottom: cs.activeTab === "cred" ? `2px solid ${V.or}` : "2px solid transparent", marginBottom: -1, background: "none", border: "none", borderLeft: "none", borderRight: "none", borderTop: "none", fontFamily: "inherit" }}
                        >
                          {"\uD83D\uDD11"} {t.credential}
                        </button>
                      </div>
                    )}

                    {/* QR pane */}
                    {hasQr && cs.activeTab === "qr" && (
                      <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10, padding: "8px 0" }}>
                        {/* QR image or placeholder */}
                        {cs.qrUrl ? (
                          <img src={cs.qrUrl} alt="QR" style={{ width: 140, height: 140, borderRadius: 8, background: "#fff", padding: 6 }} />
                        ) : (
                          <div style={S.qrBox}>
                            {cs.qrStatus === "waiting" ? renderSpinner() : renderQrPixels()}
                          </div>
                        )}
                        {/* Status + action */}
                        <div style={{ fontSize: 11, color: V.t3, display: "flex", alignItems: "center", gap: 5 }}>
                          {cs.qrStatus === "confirmed" ? (
                            <><span style={{ color: V.green }}>{"\u2713"}</span> {t.connected}</>
                          ) : cs.qrStatus === "waiting" || cs.qrUrl ? (
                            <>{renderSpinner()} {t.waitingScan}</>
                          ) : cs.qrStatus === "error" ? (
                            <button onClick={() => startChannelQr(activeId)} style={{ padding: "5px 12px", borderRadius: 6, border: `1px solid ${V.bd2}`, background: V.bg4, color: V.t1, fontSize: 11, cursor: "pointer", fontFamily: "inherit" }}>
                              {t.retry}
                            </button>
                          ) : (
                            <button onClick={() => startChannelQr(activeId)} style={{ padding: "5px 12px", borderRadius: 6, border: `1px solid ${V.bd2}`, background: V.bg4, color: V.t1, fontSize: 11, cursor: "pointer", fontFamily: "inherit" }}>
                              {t.getQr}
                            </button>
                          )}
                        </div>
                      </div>
                    )}

                    {/* Credential pane (or always show if no QR) */}
                    {(!hasQr || cs.activeTab === "cred") && fields.map((f) => (
                      <div key={f.key} style={{ marginBottom: 10 }}>
                        <div style={{ fontSize: 10, color: V.t3, letterSpacing: 0.4, marginBottom: 5, fontFamily: V.mono }}>{f.label}</div>
                        <input
                          type={f.type}
                          placeholder={f.ph}
                          value={cs.credValues?.[f.key] || ""}
                          onChange={(e) => setChCred(activeId, f.key, e.target.value)}
                          style={{ width: "100%", background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 7, padding: "7px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none" }}
                        />
                      </div>
                    ))}
                  </div>
                );
              })}

              <div style={S.navRow}>
                <button style={S.btnPrev} onClick={() => setStep(2)}>{t.prev}</button>
                <button style={S.btnNext} onClick={() => setStep(4)}>{t.next}</button>
              </div>
            </div>
          )}

          {/* ── STEP 4: Config ── */}
          {step === 4 && (
            <div style={S.card}>
              <div style={S.cardTitle}>{t.step4Title}</div>
              <div style={S.cardSub}>{t.step4Sub}</div>

              {/* Bind mode */}
              <div style={{ ...S.fLabel, marginBottom: 3 }}>
                {t.bindLabel}
                <span
                  style={{
                    fontSize: 9,
                    color: V.t3,
                    fontWeight: 400,
                    marginLeft: 4,
                  }}
                >
                  gateway.bind
                </span>
              </div>
              <div style={{ fontSize: 10, color: V.t3, marginBottom: 6 }}>
                {t.bindDesc}
              </div>
              <select
                style={{ ...S.fInput(), marginBottom: 6 }}
                value={bindMode}
                onChange={(e) =>
                  setBindMode(e.target.value as "loopback" | "all" | "custom")
                }
              >
                <option value="loopback">
                  loopback {t.bindLoopback}
                </option>
                <option value="all">
                  all {t.bindAll}
                </option>
                <option value="custom">
                  custom {t.bindCustom}
                </option>
              </select>

              {bindMode === "all" && (
                <div style={S.warnBox}>
                  {t.bindWarn}
                </div>
              )}

              {bindMode === "custom" && (
                <div style={{ marginBottom: 6 }}>
                  <div style={{ ...S.fLabel, marginBottom: 5 }}>
                    {isZh ? "\u81EA\u5B9A\u4E49 IP \u5730\u5740" : "Custom IP address"}
                  </div>
                  <input
                    type="text"
                    style={{ ...S.fInput(), marginBottom: 6 }}
                    placeholder="192.168.1.100"
                    value={customIp}
                    onChange={(e) => setCustomIp(e.target.value)}
                  />
                </div>
              )}

              {/* Port */}
              <div style={{ ...S.fLabel, marginTop: 6 }}>
                {t.portLabel}
                <span
                  style={{
                    fontSize: 9,
                    color: V.t3,
                    fontWeight: 400,
                    marginLeft: 4,
                  }}
                >
                  gateway.port
                </span>
              </div>
              <input
                type="number"
                style={{ ...S.fInput(), marginBottom: 12, marginTop: 5 }}
                value={port}
                onChange={(e) => setPort(e.target.value)}
              />

              <div style={S.infoNote}>
                <span style={{ fontSize: 13, flexShrink: 0 }}>{"\u2139"}</span>
                <span>
                  {t.configNote}
                </span>
              </div>

              <div style={S.navRow}>
                <button style={S.btnPrev} onClick={() => setStep(3)}>{t.prev}</button>
                <button style={S.btnNext} onClick={() => setStep(5)}>{t.next}</button>
              </div>
            </div>
          )}

          {/* ── STEP 5: Launch ── */}
          {step === 5 && (
            <div style={S.card}>
              <div style={S.cardTitle}>{t.step5Title}</div>
              <div style={S.cardSub}>{t.step5Sub}</div>

              {checks.length > 0 && (
                <div
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: 7,
                    marginBottom: 16,
                  }}
                >
                  {checks.map((c, i) => (
                    <div key={i} style={S.lc}>
                      <span
                        style={{
                          fontSize: 15,
                          flexShrink: 0,
                          width: 20,
                          textAlign: "center" as const,
                        }}
                      >
                        {c.icon}
                      </span>
                      <div style={{ flex: 1, fontSize: 12, color: V.t1 }}>
                        {c.label}
                      </div>
                      <div style={S.lcRes(c.status)}>
                        {c.status === "ok"
                          ? "PASS"
                          : c.status === "loading"
                            ? isZh
                              ? "..."
                              : "..."
                            : c.status === "error"
                              ? "FAIL"
                              : c.status === "warn"
                                ? "WARN"
                                : isZh
                                  ? "\u7B49\u5F85"
                                  : "wait"}
                      </div>
                    </div>
                  ))}
                </div>
              )}

              {done && (
                <div style={S.successBox}>
                  <div
                    style={{
                      fontSize: 13,
                      fontWeight: 600,
                      color: V.green,
                      marginBottom: 3,
                    }}
                  >
                    {t.ready}
                  </div>
                  <div
                    style={{
                      fontSize: 11.5,
                      color: "#2a6a56",
                      lineHeight: 1.6,
                    }}
                  >
                    {t.readyDesc}
                  </div>
                </div>
              )}

              <div style={S.navRow}>
                {!done && (
                  <button
                    style={S.btnPrev}
                    onClick={() => setStep(4)}
                    disabled={launching}
                  >
                    {t.prev}
                  </button>
                )}
                {done ? (
                  <button
                    style={{ ...S.btnNext, marginLeft: "auto" }}
                    onClick={finish}
                  >
                    {t.enterConsole}
                  </button>
                ) : (
                  <button
                    style={launching ? S.btnNextDisabled : S.btnNext}
                    onClick={runLaunch}
                    disabled={launching}
                  >
                    {launching ? (
                      <span
                        style={{
                          display: "inline-flex",
                          alignItems: "center",
                          gap: 5,
                        }}
                      >
                        <div
                          style={{
                            width: 11,
                            height: 11,
                            border: "1.5px solid rgba(255,255,255,.25)",
                            borderTopColor: "#fff",
                            borderRadius: "50%",
                            animation: "onb-spin .7s linear infinite",
                          }}
                        />
                        {t.launching}
                      </span>
                    ) : t.launch}
                  </button>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
