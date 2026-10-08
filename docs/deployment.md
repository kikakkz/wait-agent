# Relay 部署与连接操作手册

relay 公网部署 → 节点入网 → 日常管理的操作手册。设计决策与协议
细节见 [relay-design.md](relay-design.md)；WebUI 的安全配置、
magic-link 认证机制与反代部署见 [webui.md](webui.md)。本文只写
操作步骤与已落地的行为，不复述设计论证。

## A. 公网 relay 主机

### 启动 relay

```console
$ waitagent relay serve --listen 0.0.0.0:7475
```

- `--listen` 缺省 `0.0.0.0:7475`；`--authorized-nodes <dir>` 可覆盖
  白名单目录（缺省 `~/.waitagent/authorized_nodes/`）。
- 端口放行：只需 relay listen 端口（默认 7475/TCP）。所有 node 都是
  主动 outbound 连接 relay，node 侧不需要任何入站端口。
- relay 磁盘状态仅 `relay.toml`、白名单目录、token 存储，不持久化
  任何会话或流量数据。

### 生成邀请 token（在 relay 本机执行）

```console
$ waitagent relay invite                 # 一次性 token，默认 TTL 15 分钟
$ waitagent relay invite --deploy        # 批量装机可复用，默认 TTL 30 天
$ waitagent relay invite --ttl 3600      # TTL 按秒覆盖
```

- token 生成后尽力复制到剪贴板（无剪贴板的环境打印提示手工复制）。
- 一次性 token 被任意兑换尝试消耗——包括被拒绝的尝试
  （fail-closed，泄露的 token 不能重放）；deploy token 可复用至过期。
- token 状态持久化在 relay 侧，relay 重启不丢未过期 token。

### 一体启动 WebUI（可选，推荐）

```console
$ waitagent relay serve --web --web-listen 0.0.0.0:8788
```

- `--web` 在同进程内拉起 web 面（默认监听 `0.0.0.0:8788`）；
  fail-stop 单 Lifetime：任一半退出则整进程退出。
- 前置条件：本机已 `relay join` 过（`~/.waitagent/relay.toml` 存在）
  且 `~/.waitagent/webui.toml` 已配置——缺失时启动即失败，不会跑
  无头 relay。

`webui.toml` 部署时一次性填写（完整说明见 [webui.md](webui.md)）：

```toml
admin_email = "you@example.com"              # 唯一管理员邮箱
mail_auth_code = "<授权码/app password>"
public_base_url = "https://dash.example.com" # magic link 里的外部地址
# trusted_proxies = "127.0.0.1"              # 反代场景才配；默认空 = 仅直连
```

- SMTP host/port/TLS 按邮箱域名从内置服务商表自动选择，用户只填
  邮箱 + 授权码。授权码获取：QQ/163 邮箱在网页版设置中生成独立
  "授权码"（不是登录密码）；Outlook(Hotmail)/Gmail 需先开 2FA，
  再生成 app password。
- `trusted_proxies`：反代/隧道部署时把反代地址配进来，设备指纹与
  限流才能取到真实客户端 IP；默认空 = 仅直连，`X-Forwarded-For`
  被忽略。

## B. 节点入网

```console
$ waitagent relay join <relay地址> <token>   # 地址 host[:port]，缺省端口 7475
```

join 一次完成双向绑定（[relay-design.md](relay-design.md) 身份认证
与入网）：

1. token 认证入网会话——无 token 者无法建立会话，没有 MitM 窗口；
2. 会话建立后 relay 经这条已认证通道下发自己的证书指纹，node 自动
   写入 `~/.waitagent/relay.toml`（`address` + `relay_fingerprint`，
   即"钉扎"）；
3. relay 同时把该 node 的证书指纹自动加入白名单。全程无需手工抄写
   指纹。

**token 按指纹记账的含义**：设备身份 = node 自签证书
（`~/.waitagent/node.crt`）的 SHA-256 SPKI 指纹，白名单按这个指纹
建条目。换机或重装会生成新证书、得到新指纹——新指纹不在白名单里，
register 在传输层（mTLS 指纹校验）即被拒绝，旧 token 也不覆盖它。
因此换机重装必须重新 `relay invite` 拿新 token 再 `relay join`；
退役旧设备的指纹条目用 `waitagent relay remove <fingerprint>` 清理。

## C. TUI 零命令行路径

不敲命令的完整走法（Ctrl-W 弹窗，三个已落地切片的组合）：

1. **加 relay**（issue #156 切片 1，PR #157）：Ctrl-W 连接弹窗的
   Relay 区填地址 + invite token，走与 `relay join` 完全相同的
   join 引擎。若新 relay 指纹与被替换的旧钉扎不一致，默认拒绝并
   恢复原钉扎；确认换 relay 后点 "Switch anyway" 带 FORCE 重发。
   Remove 则清掉 `relay.toml`、停止 relay 长连接并复位 relay 状态。
2. **加主机选 via auto**（issue #156 切片 2，PR #158）：Connection
   卡 Via 三选 Auto/Direct/Relay，auto 为缺省。auto = 直通优先、
   relay 兜底：connect 先以短超时上限探测 direct（TCP + pin 握手），
   失败再经 relay dial。生效路径记入 `last_via_used`，host header
   标注 "via auto → relay"，不静默改写用户选择。
3. **指纹自动发现**（issue #156 切片 3，PR #159）：remote-hosts
   条目的 `tls_pin_sha256` 可省略。connect 时经 node↔relay 管理
   通道 `resolve-node` 按 host 标签（节点 register 后自动公告的
   主机名 + egress IP）唯一命中对端已注册指纹；命中即随 profile
   落盘，下次复用。未命中或多命中回退 SSH bootstrap 旧路径；
   显式 direct 的条目不查询 relay。

## D. 管理面

- **Dashboard**（[webui.md](webui.md)）：浏览器打开
  `public_base_url` → 登录页输入 `admin_email` → 邮件收到 magic
  link（TTL 10 分钟、一次性，兑换校验设备指纹——请求/点击/会话全程
  同机同网络）。签名密钥为内存临时 Ed25519 keypair，重启进程全部
  会话失效，重新走 magic link 即可。登录后：只读 dashboard（relay
  listen/uptime、连接表、usage/capacity，10s 轮询）、invite（一次性
  或 deploy、TTL 可选、token 仅显示一次）、remove（输指纹确认，
  删白名单并断开在线链路）。
- **CLI**（本机 admin socket，bootstrap 与应急通道）：
  `waitagent relay status`、`waitagent relay invite [--deploy] [--ttl]`
  、`waitagent relay remove <fingerprint>`。`relay shutdown` 只走本机
  admin socket——网络可达的通道不能停止 relay。CLI 连不上运行中的
  relay 时明确报错引导，不隐式启动。

## E. 已知边界

- **浏览器 terminal 未实现**：WebUI 内复用 OpenMirror/RawPty/Resize
  的浏览器终端是 relay Phase 1 的显式非目标（relay-design.md 任务
  10 ⬜），当前 session 接入只经 TUI。
- **公网首实操**：relay 的全部进程级 e2e 都在 docker 隔离网桥拓扑
  内运行（`scripts/e2e/relay/e2e-relay.sh`、`scripts/e2e/web/e2e-web.sh`），
  公网 relay 的首个实机部署尚无记录。容量默认值保守发布，首次公网
  部署后按自机实测修订 relay.toml 容量项。
- **Windows 宿主 e2e 未覆盖**：Windows CI 跑全部 relay 单元套件，
  docker e2e 场景天然 Linux-only；Windows 宿主的 relay 部署路径没有
  进程级 e2e 锚点。
- **Microsoft 基础认证风险**：hotmail/outlook 的 app password 依赖
  租户策略，Microsoft 在收紧基础认证；授权码失效时走 `webui.toml`
  的 `mail_custom_*` 四键自建邮件服务器逃生门（Phase 1 不做 OAuth）。
