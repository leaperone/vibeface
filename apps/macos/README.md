# Vibeface for macOS

原生 SwiftUI + WKWebView 桌面壳，复用 Node 自带 Web 客户端，降低与手机端和 `packages/client` 的分叉维护成本。窗口、Dock、菜单和设置由 macOS 原生提供；默认连接 `http://localhost:4790`，节点未运行时显示安装/启动引导。

```sh
scripts/build-macos-app.sh
open .artifacts/macos/Vibeface.app
```

产物仅写入仓库 `.artifacts/macos`，不会安装到 `/Applications`，也不会停止或修改节点数据目录。
