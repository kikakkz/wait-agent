# Relay 功能设计

跨地点主机互连。地点 1 的 node 主动 outbound 连接一台公网可达的 relay；
地点 2 的 node 同样连接 relay，并通过 relay 与地点 1 的 node 建立 session，
体验与今天的直连完全一致。直连方式继续保留，未配置 relay 时行为与现状完全一致。

## 目标与非目标

目标：

- 定位：self-hosted。名称就叫 relay；公共云版本另行命名与设计
  （随计费另行展开）。
- Phase 1（本文）：CS 星型拓扑。所有 node 与 relay 保持一条 TLS 长连接，
  跨 node 流量在该连接上以 stream 多路复用。
- 数据策略：relay 只转发，不持久化任何会话或流量数据；磁盘状态仅
  relay.toml、白名单目录、token 生成参数。流量只在内存中经帧头路由。
- 单机容量模型：可标定、可配置、可观测的准入控制，作为将来集群
  调度器的输入。
- 管理面：WebUI 是日常管理面（连接表、usage、invite、在线状态、
  浏览器内 coding）；CLI 收敛为 bootstrap 最小集（serve、join、
  一行安装命令）。
- 复用现有 node↔node 协议：之上的 TLS、AuthorityTransport、ControlPlane、
  session 机制（OpenMirror/RawPty/Resize/bootstrap）一律不动。
- 双向身份认证：relay 只接受自己的 node，node 只连自己的 relay。
- 为将来云端计费预留扩展位（API key + 订阅档位），Phase 1 不实现计费。

非目标：

- NAT 穿透 / P2P（Phase 2，仅替换传输腿，mux 与协议层复用）。
- 跨账户互访授权。不同账户的 node 经由 relay 是完全独立的，不存在互信，
  也不需要配对机制。公共 relay 集群的跨账户问题随计费功能另行设计。
- 会话数据持久化 / 录制 / 审计落盘（relay 不做，见数据策略）。
- WebUI 的浏览器端 E2E（self-hosted 形态下 web 服务可见流量，
  等同人坐在该机器前；公共云形态再升级，见计费扩展位）。

## 架构

```
site1 node ══TLS 长连接══╗                 ╔══TLS 长连接══ site 2 node
site3 node ══TLS 长连接══╣═══ relay hub ═══╣══TLS 长连接══ site 4 node
                         ╚═════════════════╝
```

- 每个 node 启动后向 relay 建一条持久连接：认证 + 注册 node_id、心跳、
  承载所有跨 node 流量。断线由 node 侧重连重注册（复用现有 reconnect
  worker 模式）。
- site2 连接 site1 = 在两条长连接上开一条虚拟流：site2 发送
  `open_stream(target_node_id)`，relay 路由，两端得到一对全双工、有序、
  可靠的 byte stream —— 与一条 TCP 语义相同。
- 集成 seam：把现有"node 拨 node"的 TCP 建立路径抽象为 `PeerConnection`
  （有序可靠全双工 byte 流）。直连 TCP 是一个实现；relay stream 是另一个
  实现。现有协议代码运行在 seam 之上，不感知传输方式。

## 协议分层

```
现有会话协议（AuthorityTransport / ControlPlane / ClientHello...）   不动
node↔node TLS（现有 rustls 双向自签证书，E2E 加密）                  不动
────────────────────────────────────────────────────────────
mux 帧层：stream open / close / data / window（逐流背压）
relay 控制帧：register / unregister / open_stream / close_stream /
             error(结构化原因码) / heartbeat
node↔relay 外层连接：TLS + 心跳
────────────────────────────────────────────────────────────
relay hub：连接表（node_id → 连接）、流路由、公平调度、逐流窗口
```

relay 只解外层 TLS 和 mux 帧头用于路由；node 间内容仍是内层 E2E TLS
密文，relay 不可信也成立。

## 身份认证与入网（已定案：邀请制）

设备身份 = node 自签证书（`~/.waitagent/node.crt`）的 SHA-256 SPKI 指纹，
复用 `src/infra/node_credentials.rs` 的既有机制。无 CA。

- node → relay：relay 持独立自签证书；node 在 `~/.waitagent/relay.toml`
  钉扎 relay 指纹，TLS 校验"对端指纹 == 配置值"，不匹配即断连。
- relay → node：relay 维护 `authorized_nodes/` 白名单目录
  （照 `authorized_operators/` 模式）；mTLS 握手要求 client 证书，
  指纹不在白名单即拒绝，拒绝发生在传输层。

入网流程（`waitagent relay invite` / `waitagent relay join`）：

1. 在 relay 侧（或任一已信任 node）执行 `waitagent relay invite`，生成
   一次性、短时有效的邀请 token，自动复制到剪贴板；批量装机可用长期
   部署 token。
2. 在新 node 执行 `waitagent relay join <relay地址> <token>`：
   - token 认证入网会话，无 token 者无法建立会话（无 MitM 窗口）；
   - 会话建立后 relay 通过该已认证通道下发自己的证书指纹，node 自动
     写入 `relay.toml`；
   - relay 同时把该 node 的证书指纹自动加入白名单。
   - 双向绑定一次完成，全程无需手工抄写指纹。

## node 生命周期

- 入网：join 后指纹写入 `authorized_nodes/` 白名单；node register 建立
  长连接，relay 连接表登记 `node_id → 连接`。
- 心跳：node 每 10 秒一帧；relay 连续 3 次未收到（30s）判定离线，清理
  连接表、释放容量。node 断线由侧重连重注册（复用现有 reconnect worker
  模式）。间隔与阈值可在 relay.toml 调整，默认值如上。
- 主动下线：node 发 unregister 控制帧优雅关闭。
- 吊销：`relay remove <fingerprint>` 从白名单删除该 node 并断开其在线
  连接；被吊销 node 后续 register 在传输层即失败（mTLS 指纹校验不在
  白名单）。
- 在线状态：register / unregister / 心跳超时事件经结构化通道推送到
  关注方的 console —— remote-hosts 条目的 sidebar 状态与 status line
  展示对端 node 在线 / 离线（与错误语义同一通道；interest 在 register
  声明 remote-hosts 或 open_stream 指向该 target 时建立）。

## 管理通道（WebUI / CLI ↔ relay）

- relay 守护进程由 `waitagent relay serve` 启动（hub：监听、连接表、
  路由、心跳、容量准入）。
- **WebUI 是日常管理面**。web 服务是一个特殊 node：持有自己证书、经
  标准 register 入网，管理操作（连接表、usage/capacity、invite、remove、
  在线状态）经它的已认证 node↔relay 长连接以远端控制流送达 relay；
  浏览器 terminal 同样复用现有 ControlPlane（OpenMirror/RawPty/Resize）
  作为又一个 observer 参与者接入各 node 的 session。
- 部署形态：web 服务与 relay 同机、一体启动（`relay serve` 默认拉起
  web 面），同仓库同二进制；两进程经 loopback 走标准 node↔relay 协议，
  不开"同进程走内存"的特例路径（本地跳微秒级，相对广域网可忽略；单一
  协议路径被所有 node 共同 dogfood；故障域与部署拓扑保持解耦，压测有
  实测依据后再评估内存捷径）。
- **CLI 收敛为 bootstrap 最小集**：`relay serve`、`relay join`，以及
  供批量装机的一行安装命令（WebUI 生成 token → node 上 paste 执行）。
- 本机 admin socket（照 `remote_node_ingress_owner_socket_path` 的
  owner-control socket 模式；Windows 用等价命名管道 / `.port` marker）
  保留为 bootstrap 与应急通道，日常管理不经过它。CLI 连不上运行中的
  relay 时明确报错引导，不隐式启动。
- 权限边界：self-hosted 单管理员形态，WebUI 可达即可管理（等同拥有
  该机器）；吊销（remove）在 WebUI 执行，不经远端 node 控制流。
  多账户/细粒度授权随公共云另行设计。

## node_id 策略

- 协议中 node_id 保持 opaque String，不改协议。
- 直连模式：沿用现有 `host#port`。
- relay 模式：node_id 直接用证书指纹（或短形式），自证明全局唯一，
  无需注册表。

## 计费扩展位（Phase 1 只留 seam，不实现）

- register 的 auth 块带版本号；认证后端可插拔：私有模式 = 白名单
  （本地 Authenticator），云模式 = API key（Cloud Authenticator 调控制面）。
- `open_stream` 过一道 Policy hook：私有模式 = allowlist；云模式 =
  额度校验（按账户并发对端数）+ 拒绝时返回结构化原因码。
- relay 集群不同步依赖计费服务：key 换签名、短 TTL 的额度凭证（JWT 风
  格），之后校验全部本地完成；计量异步批量上报。
- 公共 relay 形态：relay 用真实域名 + 公网 CA 证书（WebPKI 验域名），
  指纹钉扎仅保留给自有部署。

## 容量评估与准入控制

定位：Phase 1 单机 relay 就要有**可标定、可配置、可观测**的容量模型。
它是将来集群调度的输入 —— 调度器按
`接入决策 = capacity(能力评估) − usage(当前使用)` 分配 node 到 relay。

容量维度（单机，全部可在 relay.toml 覆盖）：

- `max_nodes`：最大并发 node 长连接数。
- `max_streams`：最大并发 stream 数。
- 转发吞吐保护阈值（bytes/s）：超限时拒绝新 open_stream（保守策略，
  压测标定后再评估是否改为收紧窗口降级）。
- 资源基线：每连接 / 每流内存与 CPU 开销随版本发布给出实测值，
  部署者按自机实测覆盖默认值。

计量与准入：

- relay hub 的连接表 / 流表本身就是实时 usage（活跃连接数、活跃流数、
  逐流窗口占用、转发字节率），计量在路由路径上完成，不另建指标系统。
- register 超 `max_nodes` → 结构化拒绝；open_stream 超 `max_streams`
  → 结构化拒绝（挂在已定案的 Policy hook 上）。
- 心跳超时回收连接即释放容量，node 重连重注册重新准入。

标定方法：e2e 压测（docker 网络隔离，见任务拆分测试项）扫描连接数 /
流数 / 吞吐拐点，产出推荐默认值；默认值保守发布，实测后修订。

可观测性：relay 子命令展示 capacity 与 usage；node 侧 register /
open_stream 被拒时拿到结构化原因码，经已定案错误通道展示到 console。

## 错误语义（quota/key/设备解绑等）

relay → node → console 的错误通道现在定义好结构：错误码 + 人类可读消息，
能显示到 sidebar item 状态与 status line，不用裸字符串。

## 配置面

- node 侧 `~/.waitagent/relay.toml`：`address`、`relay_fingerprint`（join 后
  自动写入）、心跳参数。
- relay 侧 `~/.waitagent/relay.toml`：listen 地址、admin socket 路径、
  白名单目录路径、
  token 生成参数、心跳参数（间隔默认 10s、离线阈值默认 3 次）、
  容量上限（`max_nodes`、`max_streams`、吞吐保护阈值）。
- remote-hosts 条目增加 `via = "relay" | "direct"`（显式指定，不做隐式
  回退；报错信息中给出引导）。

## 兼容性承诺

- relay 只是 dial 路径新增的一个 transport 分支；未配置 relay 时代码
  路径与现在完全一致。
- 现有直连 TCP、socks5 代理路径不动。
- 所有现有 session 协议不变。

## Phase 1 任务拆分

1. mux 帧层 + `PeerConnection` seam（现有 TCP dial 收进 seam，行为不变）
2. relay 守护进程与子命令：`relay serve`、本机 admin socket 与本地
   admin 协议、远端控制流（node 侧发起 invite）、连接表、open/close
   路由、心跳（10s / 3 次超时）、白名单认证（含 `relay remove` 吊销）
3. `relay invite` / `relay join` 入网流程（双向自动钉扎）
4. node relay client：长连接保活、按需开流、断线重注册、presence
   事件接收入 console
5. 逐流背压（window/ack）与公平调度
6. 单机容量模型：usage 计量、max_nodes/max_streams 准入、压测标定
7. 配置面：relay.toml、remote-hosts `via`、Ctrl-W 连接路径接入
8. 结构化错误语义（relay → node → console）
9. 测试：双 node + relay 的 docker 网络隔离 e2e、断连重连、多并发
   session、Windows CI、直连回归
10. WebUI：web 服务作为特殊 node 入网（浏览器 terminal 复用
    OpenMirror/RawPty/Resize + 管理面：连接表、usage、invite、remove、
    在线状态）；CLI 收敛为 bootstrap 最小集（serve、join、一行安装命令）

## Phase 2 展望

- `PeerConnection` 增加 P2P 实现：经 relay 交换地址（rendezvous）、NAT
  穿透打洞，成功后流量不过 relay；relay 退化为 registry + rendezvous。
- relay 集群 + 接入调度器：调度器依据各 relay 的 capacity 声明与 usage
  上报，按 `capacity − usage` 对 join / 重连请求做分配；调度决策短 TTL，
  relay 故障由 node 重连触发重调度；容量视图允许秒级滞后，与计费凭证
  短 TTL 本地校验同一套取舍，集群不强一致。
- 设备身份升级为必须（证书指纹体系已就绪）；控制面实现计费。
