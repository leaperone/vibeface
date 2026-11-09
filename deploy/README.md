# 官方服务容器

这里只消费已发布并校验过的 component，不编译产品，不发布 Release，也不调用外部部署仓库。

`core.Dockerfile` 的构建上下文包含 `component/`、`runtime.py` 和仓库的 `config/public-bootstrap.json`，guard 用后者核对公共节点身份。Core 制品只含原生程序和 `COMPONENT-INFO.json`。运行时使用 Ubuntu 24.04，兼容制品的 GLIBC 2.39 要求。

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
  "project": "official-node",
  "role": "node",
  "deploy_dir": "/srv/service-control/node",
  "listen": "0.0.0.0:4792",
  "data_dir": "/var/lib/official-node",
  "device_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "uid": 1001,
  "gid": 1001,
  "legacy_service": "official-node.service",
  "maintenance_socket": "/tmp/private-node/maintenance.sock"
}
```

每次配置一个服务，角色约束如下。

| role | 运行方式 | 额外配置 |
| --- | --- | --- |
| node | Core host network，官方公共节点：发现服务、开放 relay 和浏览器接入，各有限额；文件描述符上限 65536；无静态 App、无 Nginx | 上例全部 Core 字段 |
| app | 静态 App，bridge network，宿主 Nginx 把浏览器接入接口经 TLS 反代到公共节点 | 通用字段，另加 `nginx_config`、`server_name`、`node_url`、`node_certificate` |
| www | 静态 WWW，bridge network | 通用字段，另加 `nginx_config`、`server_name`，tag 使用 `www-v` 前缀 |

通用字段是 `project`、`role`、`deploy_dir`、`listen`。静态角色的 `listen` 是宿主回环端口；`node` 的 `listen` 是 `0.0.0.0:<端口>`，公共节点没有域名，网页接口的 TCP 端口只讲 TLS，同号 UDP 端口对外提供发现服务。可选 `public_url` 为 HTTPS origin，对 Core 校验现有配置中的相同值。`nginx_config` 应指向真实配置文件，例如 `/etc/nginx/conf.d/app.conf`；`server_name` 例如 `app.example.invalid`。脚本拒绝路径中的符号链接。Nginx 配置需有唯一匹配的 TLS server 和 `location /`。App 的已知静态 location 会切到容器，旧 App 的服务器级 CSP、缓存和版本头会移除。App 的 `= /api/client`、`= /api/info`、`= /api/decision` 改写为反代到 `node_url`（公共节点网页接口的 `https://<IPv4>:<端口>`），缺少的补在 `location /` 前；反代透传 `Host <server_name>`，因为公共节点只接受 App 的 Host；`node_certificate` 是公共节点数据目录里 `tls-cert.pem` 的 PEM 原文，脚本把它写到本次 release 目录的 `node.pem`，反代用 `proxy_ssl_verify on`、`proxy_ssl_trusted_certificate` 和 `proxy_ssl_name <server_name>` 钉住它；反代用 `$remote_addr` 覆盖 `X-Real-IP`，`/api/client` 另带 WebSocket 升级头并保持 1 小时。App 的 TLS server 须有 `set_real_ip_from`（Cloudflare 的 IP 段）和 `real_ip_header CF-Connecting-IP`，否则 `$remote_addr` 是 Cloudflare 的地址，公共节点按浏览器地址的限额失效；缺少，或 `set_real_ip_from` 信任 `0.0.0.0/0`、`::/0` 这类全部地址时拒绝激活。Cloudflare 须开启 WebSocket。其余非静态 location 只能是 `return 404`，旧的 `/api/relay/grant`、`/bootstrap/` 和 relay 路由须先手动删除，未知规则会拒绝激活。

公共节点对任何设备开放，guard 只接受各项都有上限的配置：`network.serve_discovery` 开启；`relay_listen` 已设、`relay_open` 开启且没有 `relay_operator_token`，`relay_rate`、`relay_max_clients`、`relay_max_clients_per_ip` 已设；`forwarding.any` 开启，设了 `forwarding.rate`、`forwarding.per_ip` 和 `forwarding.trusted_proxies`；网页和 `tls` 开启，数据目录有 `tls-cert.pem` 和 `tls-key.pem`，`public_url` 为 `https://app.vibeface.dev` 且 `allowed_origins` 包含它；没有 `manager` 和项目根目录；设备编号等于镜像里 `public-bootstrap.json` 的 `issuer`。不符合时停止激活。官方取值只够正常使用：每个 relay 连接上行 64 KiB/s、最多 512 个连接、同一地址最多 16 个，同时转发 32 个浏览器会话、另有 192 个排队、同一浏览器地址最多 8 个连接、每会话 256 KiB/s。数据目录在首次激活前按下面初始化，并先以 `legacy_service` 运行：

```sh
vibeface init --data-dir /var/lib/official-node --listen 0.0.0.0:4792 --tls \
  --public-url https://app.vibeface.dev --allowed-origin https://app.vibeface.dev \
  --serve-discovery --relay-listen 0.0.0.0:3340 --relay-url http://8.217.72.118:3340 \
  --relay-open --relay-rate 65536 --relay-max-clients 512 --relay-max-clients-per-ip 16 \
  --forward-any --forward-sessions 32 --forward-queue 192 --forward-rate 262144 \
  --forward-per-ip 8 --trusted-proxy <App 主机 IPv4>
```

`config/public-bootstrap.json` 的 `issuer` 必须是该数据目录的设备编号，`udp_host`、`udp_port` 是公共节点的公网 IP 和上面的端口；`issuer` 还是占位时 guard 拒绝激活。`init` 打印的设备编号同时写进 node 配置的 `device_id`，数据目录里 `tls-cert.pem` 的内容写进 app 配置的 `node_certificate`。

**必做：阿里云安全组。** 公共节点的 TCP `listen` 端口（上例 4792）只放行 App 主机的公网 IPv4；UDP 4792（发现服务）和 TCP 3340（relay）对公网开放。脚本不改安全组，首次激活前在控制台配好并核对。

Core 数据目录必须已经初始化，权限为 0700，UID/GID 与配置一致。脚本使用只读挂载校验身份、配置、epoch 和制品信息。维护租约成功后才停旧进程，取 `node.lock` 并离线备份到私有 `deploy_dir`。容器的 bind mount 禁止自动创建源目录。整个过程不执行 init、不修改配置、不迁移 epoch、不重置数据。

激活日志、Compose 文件、旧 Nginx 文件和离线备份均留在私有 `deploy_dir`。镜像健康需同时满足容器 running/healthy 与 Core inspect 的 running、版本和 device ID。失败时先停新容器，再恢复旧容器或原 systemd 服务，并恢复 Nginx；不会自动覆盖现有数据。中断后的非终态日志会先回滚并退出，检查旧服务后可重跑原命令。新容器无法确认停止时拒绝启动旧进程。确认后的同一请求会再次核对实际容器健康。

本地验证入口均不访问生产服务。

```sh
python3 deploy/runtime.py self-test
python3 deploy/static-config.py --self-test
python3 scripts/deploy-activate.py --self-test
```

镜像跨 Docker 存储后端传输时，接收端的不可变 ID 可能与构建端不同。导入流程应先校验传输归档的 SHA-256，再使用 Docker 返回的实际导入 ID 执行激活检查。
