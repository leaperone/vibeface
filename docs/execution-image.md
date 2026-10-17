# 共享执行镜像

共享资源的执行使用独立 Linux 容器，不挂载宿主 HOME、Provider 程序、Docker socket 或上游模型密钥。Node 负责授权、资源专用 HOME、项目挂载、执行租约及容器停止确认。仅有镜像不能绕过 Grant 或开启共享执行。

下载包包含 `scripts/execution/Dockerfile` 和 `scripts/build-execution-image.sh`。在包根目录或源码根目录运行：

```sh
bash scripts/build-execution-image.sh
cat .artifacts/execution-image/BUILD-INFO.json
```

构建需要 Docker 及 npm registry 网络。脚本使用空构建上下文，不把项目文件或本机凭证发送给构建器，不发布远程镜像。它输出本机不可变的 `sha256:` image ID，并写入 `image.id` 和构建信息。干净源码树记录 Git HEAD；下载包读取自己的 `BUILD-INFO.json`。未提交源码或无法确认来源时记录 `unknown`。

配方固定 Node 22 Bookworm 的多平台 index digest，以及 Codex `0.163.0`、Claude Code `2.1.296`。镜像包含 Node.js、`/usr/bin/python3` 及 watchdog 所需的标准库 `ctypes`，默认使用非 root 用户。CLI 安装或版本检查失败会使构建失败，不安装测试替身。

Node 的 `config.json` 中添加：

```json
{
  "execution_isolation": {
    "image": "sha256:<image.id 中的 64 位小写十六进制值>",
    "docker_bin": "/absolute/path/to/docker"
  }
}
```

这个对象放进现有配置，保留其他字段。停止 Node 后修改，再按原数据目录启动。`image` 必须使用完整 digest 或本机 image ID，不能用可变 tag。没有模型出口时容器不具有外部网络。

需要 Codex 模型调用时，在 `execution_isolation` 中配置受信模型网关：

```json
{
  "egress": {
    "gateway_container": "trusted-model-gateway",
    "gateway_port": 8080,
    "codex_model": "your-model-id"
  }
}
```

网关需由部署方独立提供和保管上游凭证。执行容器只使用受限的资源级模型出口，不取得上游密钥。不要将宿主凭证目录挂载到执行容器。共享 Cursor 模型出口目前拒绝。

镜像构建及 `--version` 检查证明真实 CLI 可启动，不证明账户认证或模型调用成功。Claude 的模型出口和真实模型调用需要另行核对。Node 程序、镜像、Provider 与实际模型结果分别验收。
