# WebUI 部署与安全配置（issue #131）

`waitagent web serve` 是 relay 的 WebUI 进程:一个持有自有节点证书
的特殊 node,经标准 node↔relay 协议入网,管理操作(状态、invite、
remove)全部经它的已认证长连接送达 relay——同机也不开进程内特例
(docs/relay-design.md 管理通道)。浏览器侧为服务端渲染(askama),
零 JS 框架/构建链。

## 启动形态

- 默认监听 `0.0.0.0:8788`(公网部署形态);启动时会打印暴露告警。
- 纯本机使用:`--listen 127.0.0.1:8788` 回环绑定,无告警。
- 前置条件:本机已 `relay join` 过(`~/.waitagent/relay.toml` 存在),
  且 `~/.waitagent/webui.toml` 已配置(缺失时启动报错并指引)。

## webui.toml(部署时一次性填写)

```toml
admin_email = "you@example.com"        # 唯一管理员邮箱
mail_auth_code = "<授权码/app password>"
public_base_url = "https://dash.example.com"   # magic link 里的外部地址
# trusted_proxies = "127.0.0.1"        # 反代场景才配;默认空 = 仅直连
# 表外域名自建邮件服务器的逃生门(全部四个键成组出现):
# mail_custom_host = "smtp.corp.example"
# mail_custom_port = 465
# mail_custom_tls = "ssl"              # ssl | starttls | none
# mail_custom_user = "you@corp.example"
```

SMTP host/port/TLS 按邮箱域名从内置服务商表选择,用户只填邮箱 +
授权码(v4 拍板):

| 域名 | SMTP host | 端口/TLS |
|---|---|---|
| qq.com | smtp.qq.com | 465 SSL |
| 163.com | smtp.163.com | 465 SSL |
| hotmail.com / outlook.com | smtp.office365.com | 587 STARTTLS |
| gmail.com | smtp.gmail.com | 465 SSL |

授权码获取指引:QQ/163 邮箱需在网页版设置中生成独立"授权码"
(不是登录密码);Outlook/Gmail 需先开 2FA,再生成 app password。
已知风险:Microsoft 在收紧基础认证,hotmail 的 app password 依赖
租户策略;失效时走 `mail_custom_*` 或等 OAuth(Phase 1 不做 OAuth)。

## 登录(magic link + 设备指纹)

1. 浏览器打开 dashboard → 无会话 → 登录页输入邮箱。**仅当输入 ==
   admin_email 才发信**(其余地址得到相同的中性回答,防轰炸/探测);
   限流 3 次/10 分钟/IP。
2. 邮件内含 `public_base_url + /auth/magic?token=...`。token 为
   Ed25519 签名的 JWT(服务端密钥 `~/.waitagent/web-auth.key`,首启
   生成,0600;备份它,轮换命令另立项),magic TTL 10 分钟、一次性
   (任何兑换尝试即消耗)。
3. 兑换时校验设备指纹:SHA-256(客户端IP[经 trusted_proxies 取真值]
   + User-Agent + JS 探针 platform/timezone/language/screen)。**同机
   约束按字面**:请求 link 的机器 == 点击的机器 == 会话全程的机器/
   浏览器/网络;换即拒(重新 magic link 即可)。心跳 30s,90s 无心跳
   会话作废;会话绝对寿命 12h。重启进程即全体会话失效(v3 明载)。

## Dashboard 与写操作

- 只读页:relay listen/uptime、连接表(node_id/idle/online)、
  usage/capacity;10s `<meta refresh>` 轮询。
- **invite**:一次性(默认)或 deploy(勾选,可复用至过期),TTL 可
  选;结果 token 仅显示一次。生成 token 经 node 通道送达 relay 落盘,
  被邀请节点照常用 `waitagent relay join <addr> <token>` 入网。
- **remove**:需输入指纹(或唯一前缀)确认;relay 删除白名单条目并
  断开其在线链路。前缀解析在 web 侧用当前连接表唯一匹配,歧义即拒。
- **CSRF**:所有写 POST 带会话级 CSRF token(登录种入模板隐藏域,
  常量时间校验);指纹中间件之外再挡一层跨站。
- **审计**:invite/remove 写入 error_log(自带毫秒时间戳),操作者记
  会话 jti 前 8 位(`waitagent __error-log` 查看)。

## 反代与公网部署

- 反代/隧道部署时,把反代地址配进 `trusted_proxies`,指纹与限流才
  取到真实客户端 IP;默认空 = 仅直连(X-Forwarded-For 被忽略)。
- 会话 cookie `HttpOnly; SameSite=Strict`,无持久化会话表(内存)。
- 邮件链接要求点击者网络与请求者一致(NAT 出口相同);手机流量点
  PC 请求的链接会被指纹拒绝——这是设计内的明确取舍。

## 应急通道保留

本机 admin socket(`relay status/invite/remove/shutdown --listen ...`)
保留为 bootstrap 与应急通道,WebUI 全部能力之外仍可用;**shutdown
只走本机 admin socket**,node 通道永久拒绝(网络可达的通道不应能
停止 relay)。CLI 连不上运行中的 relay 时明确报错引导,不隐式启动。
