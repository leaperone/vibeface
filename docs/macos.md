---
title: macOS 桌面壳
description: 在本机以独立 App 打开 Node 内嵌网页 UI。
---

# macOS 桌面壳

`apps/macos` 提供原生 **SwiftUI + WKWebView** 壳，默认加载本机 Node 的 HTTP 入口 `http://localhost:4790`（与 [node.md](node.md) 默认 `listen` 一致）。Node 仍由 `vibeface` CLI 或 `vibeface service` 运行；App 不打包、不启动 Node。

## 构建

```sh
scripts/build-macos-app.sh
open .artifacts/macos/Vibeface.app
```

产物为未签名的 `Vibeface.app`。Dock 图标来自 `apps/mobile/assets/icon` 的终端脸素材（构建脚本生成 `.icns`）。

## 使用

1. 按 [install.md](install.md) 安装并启动 Node（前台 `vibeface` 或后台 `vibeface service start`）。
2. 打开 App；若 Node 未监听，会显示离线说明与重试。
3. **Vibeface → Settings** 可改 Base URL（例如自定义 `--listen` 端口）。

菜单 **Node → Reload page** 刷新；**Use default URL** 恢复 `localhost:4790`。

## 与手机 App 的关系

手机 App（Expo + `@vibeface/client`）是跨设备客户端。macOS 壳当前复用的是 **Node 内嵌 Web UI**（`crates/node/web`），与浏览器访问本机 Node 等价，便于在本机管理同一 Node。后续若需要与手机同款的独立客户端，可评估 Expo macOS 或 Tauri 嵌入 `@vibeface/client` UI。

## CI

PR 打上 `macos` 标签或手动 `workflow_dispatch` 触发 `.github/workflows/macos.yml`，上传 `vibeface-macos-<sha>.zip` artifact。
