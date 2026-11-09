# 官方服务容器

这里只消费已发布并校验过的 component，不编译产品，不发布 Release，也不调用外部部署仓库。

`core.Dockerfile` 的构建上下文包含 `component/`、`runtime.py` 和仓库的 `config/public-bootstrap.json`，guard 用后者核对公共节点身份。Core 制品只含原生程序和 `COMPONENT-INFO.json`。运行时使用 Ubuntu 24.04，兼容制品的 GLIBC 2.39 要求。

`static.Dockerfile` 的上下文包含 `component/`、`runtime.py` 和 `nginx.conf`。App 使用 `web/`，WWW 构建时传 `--build-arg STATIC_ROOT=site`。配置从制品生成。

```sh
python3 deploy/static-config.py --role app --component .artifacts/deploy/app/component --output .artifacts/deploy/app/nginx.conf
```

App 使用制品内 `_headers` 的 CSP 和缓存规则，保留 `/_shell.html` 回退，缺失 API、SDK 和资源返回 404。静态容器以 UID/GID 101 运行，只监听容器内 8080，由 Compose 映射到配置中的宿主回环端口。

远端需有 Docker Compose、Python 3.12 或更新版本和 systemd；静态角色另需 Nginx。脚本只用 `systemctl is-active`、`is-enabled`、`start`、`stop`、`enable`、`disable`，systemd 219（CentOS 7）即可。先导入镜像，再在宿主以 root 执行 `scripts/deploy-activate.py`。将 `deploy/` 放在该脚本同级目录或父目录。镜像参数必须是已导入的 `sha256:` image ID，不使用可移动 tag。

```sh
python3 scripts/deploy-activate.py --config /run/private/deploy.json \
  --image sha256:IMAGE_ID --tag v1.0.0 --version 1.0.0 --source-sha SOURCE_COMMIT
```

`--config -` 从标准输入读私有 JSON。公共示例中的名字、路径、身份和端口均需替换为实际配置。不要把私有 JSON 提交到仓库。

```json
{
  "project": "official-node",
  "role": "node",
  "deploy_dir": "/opt/apps/vibeface-node/production",
  "listen": "0.0.0.0:4792",
  "data_dir": "/opt/apps/vibeface-node/state",
  "device_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "uid": 1001,
  "gid": 1001
}
```

替换宿主上已有的 systemd 服务时，另加 `"legacy_service": "official-node.service"` 和 `"maintenance_socket": "/tmp/private-node/maintenance.sock"`，两者要么都配，要么都不配。

每次配置一个服务，角色约束如下。

| role | 运行方式 | 额外配置 |
| --- | --- | --- |
| node | Core host network，官方公共节点：发现服务、开放 relay 和浏览器接入，各有限额；后两者经 Cloudflare 代理；文件描述符上限 65536；无静态 App、无 Nginx | 上例全部 Core 字段，可选 `legacy_service` 与 `maintenance_socket` |
| app | 静态 App，bridge network；浏览器直连公共节点，App 主机不转发任何接口 | 通用字段，另加 `nginx_config`、`server_name` |
| www | 静态 WWW，bridge network | 通用字段，另加 `nginx_config`、`server_name`，tag 使用 `www-v` 前缀 |

通用字段是 `project`、`role`、`deploy_dir`、`listen`。静态角色的 `listen` 是宿主回环端口；`node` 的 `listen` 是 `0.0.0.0:<端口>`，网页接口的 TCP 端口只讲 TLS，同号 UDP 端口对外提供发现服务。可选 `public_url` 为 HTTPS origin，对 Core 校验现有配置中的相同值。`nginx_config` 应指向真实配置文件，例如 `/etc/nginx/conf.d/app.conf`；`server_name` 例如 `app.example.invalid`。脚本拒绝路径中的符号链接。Nginx 配置需有唯一匹配的 TLS server 和 `location /`。App 的已知静态 location 会切到容器，旧 App 的服务器级 CSP、缓存和版本头会移除。网页直接从浏览器连接公共节点的 `gateway_url`，App 主机不反代任何接口：其余非静态 location（包括 `/api/client`、`/api/info`、`/api/decision`）只能是 `return 404`，旧的反代规则、`/api/relay/grant`、`/bootstrap/` 和 relay 路由须先手动删除或改成 `return 404`，未知规则会拒绝激活。

公共节点对任何设备开放，guard 只接受各项都有上限的配置：`network.serve_discovery` 开启；`relay_listen` 已设、`relay_open` 开启且没有 `relay_operator_token`，`relay_rate`、`relay_max_clients`、`relay_max_clients_per_ip` 已设；`forwarding.any` 开启，设了 `forwarding.rate` 和 `forwarding.per_ip`；`forwarding.trusted_proxies` 恰好是 Cloudflare 公布的全部 IPv4 和 IPv6 网段（`deploy/runtime.py` 的 `CLOUDFLARE`），`forwarding.client_ip_header` 是 `cf-connecting-ip`，网页接入和 relay 的限额都按 Cloudflare 给出的真实客户端地址计；网页和 `tls` 开启，数据目录有 `tls-cert.pem` 和 `tls-key.pem`，`public_url` 和 `network.relay_url` 都等于镜像里 `public-bootstrap.json` 的 `gateway_url`，`allowed_origins` 包含 `https://app.vibeface.dev`；没有 `manager` 和项目根目录；设备编号等于镜像里 `public-bootstrap.json` 的 `issuer`。不符合时停止激活。这些约束只针对官方部署，Core 本身照常支持下文的纯 IP 直连。官方取值只够正常使用：每个 relay 连接上行 64 KiB/s、最多 512 个连接、同一地址最多 16 个，同时转发 32 个浏览器会话、另有 192 个排队、同一浏览器地址最多 8 个连接、每会话 256 KiB/s。

## 官方部署：经 Cloudflare 代理

官方公共节点对外只有 `https://node.vibeface.dev`，源站 IP 不出现在网页和地址记录里；只有 UDP 发现服务直连 `config/public-bootstrap.json` 的 `udp_host:udp_port`，不经过 Cloudflare。Cloudflare 侧须满足：

- DNS：`node.vibeface.dev` 指向公共节点，开启代理（橙云）。
- Network：开启 WebSockets。relay 的 `/relay` 是 WebSocket 升级请求。
- Origin Rules 按路径分流：`/relay`、`/ping`、`/generate_204` 的目标端口改为 3340（relay，源站明文 HTTP）；其余路径改为 4792（Core 网页接口，源站只讲 TLS）。
- SSL/TLS：上面三个 relay 路径用 Configuration Rule 设为 Flexible，其余为 Full (strict)。
- 源站证书：在 Cloudflare 为 `node.vibeface.dev` 签发 Origin CA 证书，见下文。

数据目录在首次激活前按下面初始化。`CLOUDFLARE` 与 `deploy/runtime.py` 的同名列表一致；Cloudflare 公布新网段时，两处、数据目录 `config.json` 的 `forwarding.trusted_proxies` 和宿主防火墙一起改。

```sh
CLOUDFLARE="173.245.48.0/20 103.21.244.0/22 103.22.200.0/22 103.31.4.0/22 141.101.64.0/18
108.162.192.0/18 190.93.240.0/20 188.114.96.0/20 197.234.240.0/22 198.41.128.0/17 162.158.0.0/15
104.16.0.0/13 104.24.0.0/14 172.64.0.0/13 131.0.72.0/22 2400:cb00::/32 2606:4700::/32
2803:f800::/32 2405:b500::/32 2405:8100::/32 2a06:98c0::/29 2c0f:f248::/32"
proxies=(); for net in $CLOUDFLARE; do proxies+=(--trusted-proxy "$net"); done
vibeface init --data-dir /opt/apps/vibeface-node/state --listen 0.0.0.0:4792 --tls \
  --public-url https://node.vibeface.dev --allowed-origin https://app.vibeface.dev \
  --serve-discovery --relay-listen 0.0.0.0:3340 --relay-url https://node.vibeface.dev \
  --relay-open --relay-rate 65536 --relay-max-clients 512 --relay-max-clients-per-ip 16 \
  --forward-any --forward-sessions 32 --forward-queue 192 --forward-rate 262144 \
  --forward-per-ip 8 "${proxies[@]}" --client-ip-header CF-Connecting-IP
chown -R 1001:1001 /opt/apps/vibeface-node/state && chmod 0700 /opt/apps/vibeface-node/state
```

iroh 连接 relay 时把 `relay_url` 的路径换成 `/relay`，探测用 `/ping` 和 `/generate_204`，所以 `relay_url` 与 `public_url` 同为 `https://node.vibeface.dev`，由上面的 Origin Rules 分到 3340。`config/public-bootstrap.json` 的 `issuer` 必须是该数据目录的设备编号，`udp_host`、`udp_port` 是公共节点的公网 IP 和上面的端口，`gateway_url` 是上面的 `public_url`，也是网页默认的接入地址；`issuer` 还是占位时 guard 拒绝激活。`init` 打印的设备编号同时写进 node 配置的 `device_id`。

Core 只在 TCP 对端属于 `trusted_proxies` 的网段时读 `client_ip_header` 指定的单个请求头，其余对端一律按 TCP 对端地址计。relay 前置在对端可信时先读完 HTTP 请求头（最多 16 KiB、10 秒，超时即断开），按头里的地址（IPv6 按 /56）计 `relay_max_clients_per_ip`，再把读到的字节原样交给 relay。头名只读一个、不按优先级回退，因为 Cloudflare 会原样转发客户端自带的 `X-Real-IP`，nginx 也会原样转发客户端自带的 `CF-Connecting-IP`，按固定顺序读任一组合都会让另一种部署可被伪造。

`init --tls` 写的是自签占位证书。官方节点换成 Cloudflare Origin CA 证书：在 Cloudflare 的 SSL/TLS → Origin Server 为 `node.vibeface.dev` 创建证书，PEM 格式，证书和私钥只在宿主落地，然后安装到数据目录：

```sh
install -o 1001 -g 1001 -m 0644 origin-cert.pem /opt/apps/vibeface-node/state/tls-cert.pem
install -o 1001 -g 1001 -m 0600 origin-key.pem /opt/apps/vibeface-node/state/tls-key.pem
```

Origin CA 证书只有 Cloudflare 信任，有效期最长 15 年，到期前在 Cloudflare 重签并按上面覆盖即可。`tls-cert.pem` 是完整证书链，`tls-key.pem` 归 1001、权限 0600。Core 每次接受连接时检查这两个文件，内容变化后重新加载，不需要重启容器；证书与私钥暂时不配对（例如只写完一个）时继续用旧证书，5 秒后再试。

**必做：防火墙。** 阿里云安全组对公网开放 UDP 4792（发现服务）、TCP 4792 和 TCP 3340；宿主的 TCP 4792 和 3340 只放行 `CLOUDFLARE` 网段，IPv4 用 `iptables`、IPv6 用 `ip6tables`，例如：

```sh
iptables -N VIBEFACE-CF
for net in <CLOUDFLARE 中的 IPv4 网段>; do iptables -A VIBEFACE-CF -s "$net" -j ACCEPT; done
iptables -A VIBEFACE-CF -j DROP
iptables -I INPUT -p tcp -m multiport --dports 4792,3340 -j VIBEFACE-CF
```

不需要 TCP 80。脚本不改安全组和防火墙，首次激活前配好并核对：源站 IP 直连 TCP 4792 或 3340 应被丢弃。

## 自建：纯 IP 直连

没有域名、不经代理的公共节点同样受支持，适合他人自建：浏览器在连接设置里把接入地址改成 `https://<IP>:4792` 即可。这时不配 `--trusted-proxy`，网页接入和 relay 的限额都按 TCP 对端地址计；`--public-url https://<IP>:4792`，`--relay-url http://<IP>:3340`。这种部署不经本目录的 guard。

浏览器不信任 `init --tls` 的自签占位证书，由宿主 root 的 acme.sh（要写 0700 的数据目录并改属主）向 Let's Encrypt 申请 IP 证书（`shortlived` profile，约 6 天有效，acme.sh 按 ARI 大约每 3 天续期），HTTP-01 验证走宿主 TCP 80 上指向 `/var/www/acme` 的 webroot，TCP 80 必须能从公网访问：

```sh
acme.sh --issue --server letsencrypt -d 203.0.113.7 --certificate-profile shortlived -w /var/www/acme
acme.sh --install-cert -d 203.0.113.7 \
  --fullchain-file /opt/apps/vibeface-node/state/tls-cert.pem \
  --key-file /opt/apps/vibeface-node/state/tls-key.pem \
  --reloadcmd "chown 1001:1001 /opt/apps/vibeface-node/state/tls-cert.pem /opt/apps/vibeface-node/state/tls-key.pem && chmod 0600 /opt/apps/vibeface-node/state/tls-key.pem"
```

续期写入后 Core 按上文的热加载换用新证书，不需要重启。防火墙对公网开放 TCP 80、TCP 4792、UDP 4792 和 TCP 3340。

## 激活

Core 数据目录必须已经初始化，权限为 0700，UID/GID 与配置一致。脚本使用只读挂载校验身份、配置、epoch、TLS 文件和制品信息。之后分三种情况：

- 已有容器部署（`deploy_dir` 里有 `current.json`）：旧容器的维护租约成功后才停它。
- 配了 `legacy_service`：旧 systemd 服务的维护租约成功后才停它。
- 都没有，即全新安装：要求本项目没有正在运行的容器，不停任何进程。

然后取 `node.lock` 离线备份到私有 `deploy_dir`，再用 Compose 启动新容器并检查健康。容器的 bind mount 禁止自动创建源目录。整个过程不执行 init、不修改配置、不迁移 epoch、不重置数据。

激活日志、Compose 文件、旧 Nginx 文件和离线备份均留在私有 `deploy_dir`。镜像健康需同时满足容器 running/healthy 与 Core inspect 的 running、版本和 device ID。失败时先停新容器，再恢复旧容器或原 systemd 服务，并恢复 Nginx；全新安装没有旧服务，只停新容器；不会自动覆盖现有数据。中断后的非终态日志会先回滚并退出，检查旧服务后可重跑原命令。新容器无法确认停止时拒绝启动旧进程。确认后的同一请求会再次核对实际容器健康。

本地验证入口均不访问生产服务。

```sh
python3 deploy/runtime.py self-test
python3 deploy/static-config.py --self-test
python3 scripts/deploy-activate.py --self-test
```

镜像跨 Docker 存储后端传输时，接收端的不可变 ID 可能与构建端不同。导入流程应先校验传输归档的 SHA-256，再使用 Docker 返回的实际导入 ID 执行激活检查。
