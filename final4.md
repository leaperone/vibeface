# App Nightly 发布接入

- PR：https://github.com/leaperone/vibeface/pull/420
- 触发链路：版本 tag `v*` 只触发 `ci.yml`；`ci.yml` 的 `app-build` 调用可复用的 `release-apps.yml`，构建 macOS/Android 并上传 workflow artifacts；原 `publish` job 下载这些 artifacts，与三个 Node tarball 一起校验 sha256、资产数量和逐个下载比对，再创建/发布同一个 GitHub Release。`release-apps.yml` 不再监听 tag，避免双跑和 draft 冲突。
- 手动补发：`release-apps.yml` 保留 `workflow_dispatch`，针对已有 tag 构建后只执行 `gh release upload --clobber`，不会把已发布 Release 改回 draft。
- 版本：macOS `CFBundleShortVersionString` 使用去掉 `v` 的完整版本；`CFBundleVersion` 使用 nightly 日期序号推导的数字。Android `versionName` 使用完整版本，`versionCode` 使用自 2020-01-01 起的天数乘 100 加 nightly 序号。
- 资产：`vibeface-<version>-macos-arm64.zip`、`.dmg` 及各自 `.sha256`；`vibeface-<version>-android.apk`、`.sha256`、`.provenance.json`。
- 签名：macOS Developer ID / Apple API secrets 全部存在时签名、公证；缺少时日志提示并继续构建。
- 构建失败策略：App 构建失败时 `app-build` 失败，`publish` 不运行，Release 不会被正式发布。
- 本地验证：actionlint 通过；macOS nightly App 构建通过并检查 plist 版本为 `0.0.1-nightly.20261031.13` / `2026103113`；`scripts/ci.py --self-test` 受本机未安装 rustc 阻塞。
- PR CI：运行 `41173173148`；截至记录时 plan、quick-check、crypto-web、web、www、delivery、rust-check、macOS App 构建均通过，Android App、rust-fast 和 Linux package 仍在运行。
