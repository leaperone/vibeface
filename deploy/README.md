# 官方服务容器

这里只消费已发布并校验过的 component，不编译产品，不发布 Release，也不调用外部部署仓库。

`core.Dockerfile` 的构建上下文包含 `component/` 与 `runtime.py`。Core 制品只含原生程序和 `COMPONENT-INFO.json`。运行时使用 Ubuntu 24.04，兼容制品的 GLIBC 2.39 要求。

`static.Dockerfile` 的上下文包含 `component/`、`runtime.py` 和 `nginx.conf`。App 使用 `web/`，WWW 构建时传 `--build-arg STATIC_ROOT=site`。配置从制品生成。

```sh
python3 deploy/static-config.py --role app --component .artifacts/deploy/app/component --output .artifacts/deploy/app/nginx.conf
```

App 使用制品内 `_headers` 的 CSP 和缓存规则，保留 `/_shell.html` 回退，缺失 API、SDK 和资源返回 404。静态容器以 UID/GID 101 运行，只监听容器内 8080，由 Compose 映射到配置中的宿主回环端口。

远端需有 Docker Compose、Python 3、Nginx 和 systemd。先导入镜像，再在宿主以 root 执行 `scripts/deploy-activate.py`。将 `deploy/` 放在该脚本同级目录或父目录。镜像参数必须是已导入的 `sha256:` image ID，不使用可移动 tag。

```sh
python3 scripts/deploy-activate.py --config /run/private/deploy.json \
  --image sha256:IMAGE_ID --tag v1.0.0 --version 1.0.0 --source-sha SOURCE_COMMIT
```

`--config -` 从标准输入读私有 JSON。公共示例中的名字、路径、身份和端口均需替换为实际配置。不要把私有 JSON 提交到仓库。

```json
{
  "project": "official-entry",
  "role": "entry",
  "deploy_dir": "/srv/service-control/entry",
  "listen": "127.0.0.1:4900",
  "data_dir": "/var/lib/official-entry",
  "device_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "uid": 1001,
  "gid": 1001,
  "legacy_service": "official-entry.service",
  "maintenance_socket": "/tmp/private-node/maintenance.sock"
}
```

每次配置一个服务，角色约束如下。

| role | 运行方式 | 额外配置 |
| --- | --- | --- |
| discovery | Core host network，discovery-only，`--no-web` | 上例全部 Core 字段 |
| entry | Core host network，forward/grants，保留 API，无静态 App | 上例全部 Core 字段 |
| relay | Core host network，forward/operator-token，`--no-web` | 全部 Core 字段，另加 `nginx_config`、`server_name` |
| app | 静态 App，bridge network | 通用字段，另加 `nginx_config`、`server_name` |
| www | 静态 WWW，bridge network | 通用字段，另加 `nginx_config`、`server_name`，tag 使用 `www-v` 前缀 |

通用字段是 `project`、`role`、`deploy_dir`、`listen`。可选 `public_url` 为 HTTPS origin，对 Core 校验现有配置中的相同值。`nginx_config` 应指向真实配置文件，例如 `/etc/nginx/conf.d/app.conf`；`server_name` 例如 `app.example.invalid`。脚本拒绝路径中的符号链接。Nginx 配置需有唯一匹配的 TLS server 和 `location /`。App 的已知静态 location 会切到容器，API、bootstrap 和 relay location 原文保留，旧 App 的服务器级 CSP、缓存和版本头会移除。Relay 只将该 TLS server 的 `location /` 改为 404，原有 `/relay`、`/ping`、`/generate_204` 共用路由保留。未知静态规则会拒绝激活。

Core 数据目录必须已经初始化，权限为 0700，UID/GID 与配置一致。脚本使用只读挂载校验身份、配置、epoch 和制品信息。维护租约成功后才停旧进程，取 `node.lock` 并离线备份到私有 `deploy_dir`。容器的 bind mount 禁止自动创建源目录。整个过程不执行 init、不修改配置、不迁移 epoch、不重置数据。

激活日志、Compose 文件、旧 Nginx 文件和离线备份均留在私有 `deploy_dir`。镜像健康需同时满足容器 running/healthy 与 Core inspect 的 running、版本和 device ID。失败时先停新容器，再恢复旧容器或原 systemd 服务，并恢复 Nginx；不会自动覆盖现有数据。中断后的非终态日志会先回滚并退出，检查旧服务后可重跑原命令。新容器无法确认停止时拒绝启动旧进程。确认后的同一请求会再次核对实际容器健康。

本地验证入口均不访问生产服务。

```sh
python3 deploy/runtime.py self-test
python3 deploy/static-config.py --self-test
python3 scripts/deploy-activate.py --self-test
```

镜像跨 Docker 存储后端传输时，接收端的不可变 ID 可能与构建端不同。导入流程应先校验传输归档的 SHA-256，再使用 Docker 返回的实际导入 ID 执行激活检查。
