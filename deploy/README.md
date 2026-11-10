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
  "tls_dir": "/opt/apps/vibeface-node/tls",
  "device_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "uid": 1001,
  "gid": 1001
}
```

替换宿主上已有的 systemd 服务时，另加 `"legacy_service": "official-node.service"` 和 `"maintenance_socket": "/tmp/private-node/maintenance.sock"`，两者要么都配，要么都不配。

每次配置一个服务，角色约束如下。

| role | 运行方式 | 额外配置 |
| --- | --- | --- |
| node | Core host network，官方公共节点：全网 bootstrap 节点、开放 relay 和浏览器接入，各有限额；文件描述符上限 65536；无静态 App、无 Nginx | 上例全部 Core 字段，可选 `legacy_service` 与 `maintenance_socket` |
| app | 静态 App，bridge network；浏览器直连公共节点，App 主机不转发任何接口 | 通用字段，另加 `nginx_config`、`server_name` |
| www | 静态 WWW，bridge network | 通用字段，另加 `nginx_config`、`server_name`，tag 使用 `www-v` 前缀 |

通用字段是 `project`、`role`、`deploy_dir`、`listen`。静态角色的 `listen` 是宿主回环端口；`node` 的 `listen` 是 `0.0.0.0:<端口>`，网页接口的 TCP 端口直接对公网只讲 TLS，前面没有代理，同号 UDP 端口是 iroh 端口，其他 Node 经它加入全网。可选 `public_url` 为 HTTPS origin，对 Core 校验现有配置中的相同值。`nginx_config` 应指向真实配置文件，例如 `/etc/nginx/conf.d/app.conf`；`server_name` 例如 `app.example.invalid`。脚本拒绝路径中的符号链接。Nginx 配置需有唯一匹配的 TLS server 和 `location /`。App 的已知静态 location 会切到容器，旧 App 的服务器级 CSP、缓存和版本头会移除。网页直接从浏览器连接公共节点的 `gateway_url`，App 主机不反代任何接口：其余非静态 location（包括 `/api/client`、`/api/info`、`/api/decision`）只能是 `return 404`，旧的反代规则、`/api/relay/grant`、`/bootstrap/` 和 relay 路由须先手动删除或改成 `return 404`，未知规则会拒绝激活。

公共节点对任何设备开放，guard 只接受各项都有上限的配置：`network.mesh.enabled` 开启；`relay_listen` 和 `relay_url` 已设、`relay_open` 开启且没有 `relay_operator_token`，`relay_rate` 不超过 16384、`relay_max_clients` 不超过 128、`relay_max_clients_per_ip` 不超过 8；`forwarding.any` 开启，设了 `forwarding.rate` 和 `forwarding.per_ip`，没有 `forwarding.trusted_proxies`（浏览器直连，限额按真实对端地址计）；网页和 `tls` 开启，`tls_dir` 的证书和私钥以只读方式挂到数据目录的 `tls-cert.pem` 和 `tls-key.pem` 上、属主是 root，`public_url` 等于镜像里 `public-bootstrap.json` 的 `gateway_url`，`allowed_origins` 包含 `https://app.vibeface.dev`；没有 `manager` 和项目根目录；设备编号等于镜像里 `public-bootstrap.json` 的 `issuer`。不符合时停止激活。官方取值只够正常使用：每个 relay 连接上行 16 KiB/s、最多 128 个连接、同一地址最多 8 个，同时转发 32 个浏览器会话、另有 192 个排队、同一浏览器地址最多 8 个连接、每会话 256 KiB/s。香港主机总上行约 3 Mbit/s，主机上的 `vibeface-relay-shape.service` 用 `tc` 把 relay 端口 3340 的出口限在 500 kbit/s，即总带宽的六分之一。浏览器接入端口本身最多接 1024 个连接，同一地址（IPv4 地址或 IPv6 /56，与 relay 相同）最多 32 个，超出的连接不读直接关闭；TLS 握手 10 秒内未完成、请求头 20 秒内未读完（包括两个请求之间的空闲）的连接被关闭，升级成 WebSocket 后不受这个超时影响。这些值写在 Core 里，不随配置变化。数据目录在首次激活前按下面初始化：

```sh
vibeface init --data-dir /opt/apps/vibeface-node/state --listen 0.0.0.0:4792 --tls \
  --public-url https://node.vibeface.dev:4792 --allowed-origin https://app.vibeface.dev \
  --relay-listen 0.0.0.0:3340 --relay-url http://node.vibeface.dev:3340 \
  --relay-open --relay-rate 16384 --relay-max-clients 128 --relay-max-clients-per-ip 8 \
  --forward-any --forward-sessions 32 --forward-queue 192 --forward-rate 262144 \
  --forward-per-ip 8
vibeface config set --data-dir /opt/apps/vibeface-node/state mesh.advertise '["8.217.72.118:4792"]'
touch /opt/apps/vibeface-node/state/node.lock
chown -R 1001:1001 /opt/apps/vibeface-node/state && chmod 0700 /opt/apps/vibeface-node/state
```

容器里看不到宿主公网 IP，`mesh.advertise`（即 `network.mesh.advertise`）写明公网地址，全网记录和节点链接才带上它；guard 不检查这一项。已部署的节点在容器里执行 `vibeface config set --data-dir /data mesh.advertise '["8.217.72.118:4792"]'` 后重启容器。

`init` 不建 `node.lock`，Node 第一次启动才建；激活脚本和 guard 都要求它已存在，用它做离线锁，所以初始化后手动建好。

`config/public-bootstrap.json` 的 `issuer` 必须是该数据目录的设备编号，`udp_host`、`udp_port` 是公共节点的公网 IP `8.217.72.118` 和上面的端口，`relay_url` 必须与上面的 `--relay-url` 相同，Core 用这四项生成默认 bootstrap 节点，每台默认入网的 Node 都把这个 relay 固定留在 relay 列表里；guard 只核对 `public_url` 等于 `gateway_url` 和设备编号等于 `issuer`，不核对 `relay_url` 和 UDP 地址，改动时两处一起改。`gateway_url` 是上面的 `public_url`，也是官方页面和 App 默认的接入地址。网页入口开启、`forwarding.any` 打开且 `public_url` 是公网 HTTPS 时 Node 宣告 `gateway` 角色；开放 relay 经邻居从外部探测通过后才宣告 `relay` 角色。`node.vibeface.dev` 只有一条指向 `8.217.72.118` 的 DNS-only A 记录（不开 Cloudflare 代理），浏览器、relay 和全网 UDP 都直连这台主机；`issuer` 还是占位时 guard 拒绝激活。`init` 打印的设备编号同时写进 node 配置的 `device_id`。

`init --tls` 写的是自签占位证书，浏览器不信任它；部署后它只是挂载点，容器里看到的是 `tls_dir` 的文件。真实证书由宿主 root 的 acme.sh 向 Let's Encrypt 申请，写进 root 所有的 `tls_dir`，不碰 1001 可写的数据目录，因此不会被数据目录里的符号链接引到别处。HTTP-01 验证走宿主 TCP 80 上指向 `/var/www/acme` 的 webroot。先建好目录和两个文件，属主和权限此后保持不变：

```sh
install -d -o root -g 1001 -m 0750 /opt/apps/vibeface-node/tls
install -o root -g root -m 0644 /dev/null /opt/apps/vibeface-node/tls/tls-cert.pem
install -o root -g 1001 -m 0640 /dev/null /opt/apps/vibeface-node/tls/tls-key.pem
```

官方节点用域名，申请普通域名证书（90 天有效，acme.sh 自动续期）：

```sh
acme.sh --issue --server letsencrypt -d node.vibeface.dev -w /var/www/acme
acme.sh --install-cert -d node.vibeface.dev \
  --fullchain-file /opt/apps/vibeface-node/tls/tls-cert.pem \
  --key-file /opt/apps/vibeface-node/tls/tls-key.pem
```

自建节点没有域名时，用纯 IP 直连 `https://<IP>:4792`，申请 Let's Encrypt 的 IP 证书（`shortlived` profile，约 6 天有效，acme.sh 按 ARI 大约每 3 天续期），`--install-cert` 同上，只把 `-d` 换成该 IP：

```sh
acme.sh --issue --server letsencrypt -d <IP> --certificate-profile shortlived -w /var/www/acme
```

`tls-cert.pem` 是完整证书链，属 root、权限 0644；`tls-key.pem` 属 root:1001、权限 0640，容器用户只能读。`tls_dir` 属 root:1001、权限 0750。激活脚本在宿主上核对这些属主和权限，容器里的 guard 再核对两个文件来自只读挂载、属主是 root、组和其他用户不可写。两个文件按单文件 bind mount 挂进容器，挂载跟随 inode：续期必须原地改写文件内容。acme.sh 的 `--install-cert` 用 `cat >` 原地写，inode 不变，符合要求；不要用先写临时文件再 `mv` 替换的方式，否则容器继续看到旧文件，直到重新激活。Core 每次接受连接时检查这两个文件，内容变化后重新加载，不需要重启容器，也不需要 reloadcmd；证书与私钥暂时不配对（例如只写完一个）时继续用旧证书，5 秒后再试。

**必做：阿里云安全组。** TCP 4792（浏览器接入，只讲 TLS）、UDP 4792（iroh 与全网）和 TCP 3340（relay）对公网开放；TCP 80 供 acme.sh 签证书。脚本不改安全组，首次激活前在控制台配好并核对。

Core 数据目录必须已经初始化，权限为 0700，UID/GID 与配置一致。`tls_dir` 按上文准备好，不在数据目录或 `deploy_dir` 里面。脚本使用只读挂载校验身份、配置、epoch、TLS 文件和制品信息。之后分三种情况：

- 已有容器部署（`deploy_dir` 里有 `current.json`）：旧容器的维护租约成功后才停它。Core 角色只要求旧容器在记录的镜像上运行，不要求健康检查通过，因为新版的配置格式可能在激活前已经写入，旧程序的健康检查会重读配置而失败；租约回应核对运行中 Core 的设备编号。这时自动回滚的旧容器也读不了新配置，需要手动恢复激活前在本地留档的配置。
- 配了 `legacy_service`：旧 systemd 服务的维护租约成功后才停它。
- 没有 `current.json`，也没有配置 `legacy_service`，即全新安装：要求本项目没有正在运行的容器，并且宿主 systemd unit 文件和 drop-in（`/etc`、`/run`、`/usr/local/lib`、`/usr/lib`、`/lib` 下的 `systemd/system`）的任何字段都没有引用 `data_dir`；有引用就拒绝激活，提示改为配置 `legacy_service`。不停任何进程。

然后取 `node.lock` 离线备份到私有 `deploy_dir`，再用 Compose 启动新容器并检查健康。容器的 bind mount 禁止自动创建源目录。整个过程不执行 init、不修改配置、不迁移 epoch、不重置数据。

新版改了配置格式或表结构时，部署前手动迁移。先把旧 `config.json` 和数据库快照拷到本地留档，再在运行中的容器里原子写入新格式的配置，然后执行部署。表结构变化而 epoch 不变时，新 Core 会报 `data_schema_mismatch` 并列出缺失的表或列。只缺新增列或新索引时，停掉 Node，按新版 `INITIAL_SCHEMA` 里的定义对该库执行对应的 `ALTER TABLE … ADD COLUMN` 和 `CREATE INDEX`，再启动。其他不匹配（列类型、主键、删除的列）没有离线修复，只能按 epoch 规则清空重建数据目录。

激活日志和 Compose 文件留在私有 `deploy_dir`。旧 Nginx 文件和离线备份只供激活中回滚；确认新容器后删除，同时删除其他旧 release 目录和上一版镜像，主机只保留当前版本。需要留档时在激活前把数据拷到本地。镜像健康需同时满足容器 running/healthy 与 Core inspect 的 running、版本和 device ID。失败时先停新容器，再恢复旧容器或原 systemd 服务，并恢复 Nginx；全新安装没有旧服务，只停新容器；不会自动覆盖现有数据。中断后的非终态日志会先回滚并退出，检查旧服务后可重跑原命令。新容器无法确认停止时拒绝启动旧进程。确认后的同一请求会再次核对实际容器健康。

本地验证入口均不访问生产服务。

```sh
python3 deploy/runtime.py self-test
python3 deploy/static-config.py --self-test
python3 scripts/deploy-activate.py --self-test
```

镜像跨 Docker 存储后端传输时，接收端的不可变 ID 可能与构建端不同。导入流程应先校验传输归档的 SHA-256，再使用 Docker 返回的实际导入 ID 执行激活检查。
