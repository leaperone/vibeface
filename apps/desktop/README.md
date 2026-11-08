# vibeface-desktop

Electron 宿主：一个受信任的本机启动器，加上一个只加载 Core 自己提供界面的业务窗口。

## 两个窗口

- **单窗口客户端**（`vibeface-app://app/`）：内置 React 业务界面；本机节点管理在 App 设置 `/settings/desktop-node`，经受限 client preload 调用主进程。
  负责选择数据目录、初始化、启动与停止本机 Core、仅连接已有服务、打开业务窗口和桌面设置。
- **业务窗口**：`sandbox`、`contextIsolation` 为真，`nodeIntegration` 为假，**没有 preload，也没有任何原生桥**。
  它加载 Core 在 `local_listen`（默认 `http://127.0.0.1:4791`）提供的真实界面。
  通知、文件选择和下载走 W3C API，由主进程的权限与下载钩子接住。

## 业务地址如何确定

`vibeface-core --control` 的 `access` 返回的 URL 建立在公开监听端口上（`http://localhost:4790`），
但 `/api/local/exchange` 和本机会话 Cookie 只存在于 `local_listen` 监听器，并且它按字面 `Host` 匹配。
`src/main/business-url.ts` 因此把 `access` 的路径与一次性 `#local=` / `#bootstrap=` 片段原样搬到
`inspect.local_listen` 报告的私有 origin 上，并校验回环地址与 `node=` 对应的设备。
片段不写入设置、不打日志。公网入口节点按其发布的 HTTPS 地址直接打开，不做改写。

## 生命周期

`OwnedChild` 与 `Attached` 是两种不同的类型，而不是 pid 加布尔值。

- 自己启动的 Core 带 `--desktop-parent-stdin`，父进程持有它的 stdin。退出或崩溃产生 EOF，Core 正常关闭；
  若 EOF 超时，会按 `SIGTERM -> SIGKILL` 分阶段等待确认退出；只有确认退出才释放 owned 状态。
- 仅连接的服务永远不会被停止，退出 Vibeface 不影响它。
- 启动前先 `inspect`；目录上已有运行中的 Core 时只提示改用仅连接，不接管、不清理任何已有数据。

## 邀请激活时序

`open-url`、`second-instance` 和初始 `argv` 只接受解析后的 `join` 邀请，先在内存队列里去重并限长缓存。
队列在 `app.whenReady()` 与 `coreSession.restore()` 完成后统一消费；若当时还没连上本机 Core，邀请会保留并在会话变化后重试，不写入设置文件。

## 命令

```
pnpm --dir apps/desktop typecheck   # tsconfig.json（主进程/preload/测试）+ tsconfig.launcher.json（渲染层）
pnpm --dir apps/desktop test        # vitest
pnpm --dir apps/desktop build       # dist/main/index.cjs, dist/preload/launcher.cjs, dist/launcher/**
pnpm --dir apps/desktop package     # scripts/build-desktop.mjs（由交付侧拥有）
node apps/desktop/scripts/make-icons.mjs   # 仅 macOS：从品牌矢量重新生成已提交的图标
```

## 运行时与开关

打包后运行时固定为 `process.resourcesPath/runtime`，其中的 `vibeface-core[.exe]` 是唯一可执行的程序。
下列变量**只在未打包时**生效，打包产物不会读取它们：

| 变量 | 作用 |
|------|------|
| `VIBEFACE_DESKTOP_RUNTIME` | 未打包时的运行时目录 |
| `VIBEFACE_DESKTOP_FAKE_CONTROL` | 测试夹具替代 `vibeface-core --control` |
| `VIBEFACE_DESKTOP_FAKE_CORE` | 测试夹具替代常驻 Core |

命令行：`--profile-dir <绝对路径>` 把设置、Chromium 配置和自己启动的 Core 与真实用户目录隔离；
`--attach` 让这次运行只连接，不启动也不停止任何 Core。
