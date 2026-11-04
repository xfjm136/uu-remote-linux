# 会话内中转切换修复 / Relay fallback



## 2026-11-04：主动通知、重启发起及真实 TURN 迁移

本轮基线为 `a0a5ccd4c0c0ca65e66a13daf7e18a0c2181c4d1`，保留已有 relay 分支全部更新。

补齐的官方链路为：`be-controlled` 云端参数 → RTCP 质量采样 → `forward_setting/switch_network_notify` → 对端修改候选策略并发送 `restart_ice` offer → 原会话应用 answer → TURN 数据路径。不是监测端直接无条件发送新 offer。

新增一手反编译依据（同一官方隔离副本）：

- `sub_180AED920`：检查开关和配置；无效参数为 loss < 10 或 latency < 200；至少 15 个样本才比较，丢包使用 `int(avg * 100) > threshold`，延迟使用严格大于。触发时清空窗口。这里的 10/200 是参数有效性下限，不是写死的触发阈值。
- `sub_180DD4090`：采样/通知周期为 1 秒。
- `sub_1809CD8F0`：发送 `forward_setting`，类型为 `switch_network_notify`。
- `sub_1809CFCF0`：通知字段为 `transport_type`、`ice_id`、`attempt_switch_type`。自动通知 transport=1，attempt=1 为优先中转，attempt=2 为 TLS 中转。
- `sub_180DEE2C0` / `sub_180DF1B90`：接收通知后在原连接更新配置，创建重启 offer。第一阶段允许云端 TURN 列表，第二阶段通过 `sub_1809CC720` 只保留 `turns:`。

实现位置：`src/relay.rs` 解析云端开关及阈值、按 SSRC 维护窗口并编码/解析精确通知；`src/relay_session.rs` 按 1 Hz 消费新 RTCP ReceiverReport 样本并发通知，接收通知后发起 relay-only ICE 重启，处理本地主动重启的 answer。协商串行化，未就绪或已有 offer 时拒绝重复发起；没有对应云端 TURN 配置时保留原连接并报固定类别错误。

`SessionIce` 保留本会话云端服务器和凭据；没有内置中转地址或个人凭据。WebRTC/ICE 的新接口只修改下一代收集配置，保留 AgentConn、mux、DTLS 和 SCTP。库的 RTP 启动逻辑同时修正为仅启动已协商的发送方向，防止工具会话中尚未使用的视频轨使重启失败。原来的接收式 `restart_ice` 流程保留。

新增真实 TURN 回归创建完整的合成 TURN 服务、完成认证 allocation：先确认 route=direct，收到切网请求后强制本地主机候选为 relay，交换新 SDP/candidate，确认 route=relay，并验证**原 CONTROL/SCTP 通道**仍能 Echo 往返。测试还验证重复请求不会并发启动协商。TURN 服务库 0.7.1 在删除 allocation 时误关闭共享 listener；夹具中对 listener 的 close 做共享包装并在测试结束显式关闭，避免把服务端缺陷误判为客户端问题。没有通过跳过 allocation 或伪造候选让测试通过。

### 复刻范围与仍需验证的差异

已按证据实现上述协议及主动切换链路，不能声称闭源实现已逐项完全等价：官方媒体统计聚合和 `possible_auto_switch_*` 策略尚未完整还原。本实现采用最近 15 个有效 RTCP 样本的窗口及 RTCP RTT；新报告不足时不重复采样旧数据。只有云端 `auto_switch_network=true` 且 force 阈值有效时自动触发。TLS 已完成协商并恢复连接后停止继续升级，不在无证据情况下发明新的降级协议。

真实 Windows 控制端与网易云 TURN 的弱网端到端验收仍未执行。本地真实 UDP TURN 迁移不能替代 TLS 中转、移动网络、官方重试/超时、同时发起重启等场景的验收。没有修改或启动原安装目录的官方程序，也没有发布其二进制或反编译源码。

本轮验证：主机 **318 通过、8 跳过、0 失败**；ICE 库 **102 通过、0 失败**；viewer 串行 **182 通过、0 失败**。一次并行 viewer 测试在未改动的 WebSocket 测试握手处失败，串行全量复核通过；未将该失败算作通过。所有 Linux 测试位于 `/tmp/uu-relay-replica`。

## 2026-11-04：补齐官方 `restart_ice` 信令（基于更新后的分支）

修复基线为 `0221f453530ccbb65ecbeb08d8e8f2ca9ceacde6`（v0.2.48），保留分支已有的 UI、防窥、媒体等更新。上次补丁只修复候选收集和重提名，实际弱网切换仍未得到用户验证；这次静态反编译确认还漏掉了官方自定义的协商类型。

官方 4.44.0.3919 隔离副本的调用依据：

- `streamer.dll!sub_180DF1B90` 设置连接配置并创建新 offer，随后发送 `restart_ice`，诊断包含 `ice_transport_type`。
- `sub_180A03942` 将 SOAC `data.type=restart_ice` 与 offer/answer 一样解析 SDP（支持 gzip）。
- `sub_180DE3D80` 明确接受 `offer` **或** `restart_ice` 并将 SDP 交给已有连接。
- `sub_180DDBFC0` 依据 restart 标志选择应答类型：普通协商为 `answer`，重启协商为 `restart_ice`。

原 Linux 主循环只处理 `offer` 和 `candidate`，导致中转切换发来的 `restart_ice` 被忽略。现在 `src/main.rs` 按 ice_id 找到原会话，调用 `src/webrtc.rs::answer_restart` 在原 PeerConnection 上应用新 offer 并回复 `restart_ice`。不会走新建桌面连接的替换/关闭逻辑，也不会消费下一次 be-controlled 的配置。若重启消息未重复携带 client_id/app_control_id，会保留原会话上下文；未知会话只记录固定警告并忽略。

新增真实虚拟网络通信回归：先通过 CONTROL 收发 Echo，再生成不同 ICE 凭据的 restart offer，重新交换候选和 SDP，验证双方恢复 Connected、仍复用原 PeerConnection/CONTROL 通道，且重启后的 Echo 能往返。它验证协商后仍能传输，不能替代官方客户端经云端 TURN 的真实弱网验收。测试中必须等待**新一代** ICE 收集完成；沿用上一代已完成的 promise 会造成候选未交换的假失败。

本次在 `/tmp/uu-relay-v48` 验证：主机完整测试 **314 通过、8 跳过、0 失败**；viewer **182 通过、0 失败**。新增的重启通信回归已通过。没有修改、注入或重启已安装的官方 UU 客户端。

## 修复的缺口

原实现已有云端 TURN 配置及 UDP 中转，并非完全没有中转。这里修复两个具体问题：

1. `src/session_ice.rs` 将 TCP/TLS TURN 收集与 `UU_USE_PROXY=1` 绑定，普通网络即使收到了云端 TCP/TLS TURN 地址也不会使用。现在默认启用直连 TURN TCP/TLS；代理开关只决定是否走已有 HTTP CONNECT 代理。UDP TURN 保留，服务器及凭据仍完全来自本次云端连接配置。
2. `vendor/webrtc-ice/src/agent/agent_selector.rs` 在已有 selected pair 时忽略新路径提名，且丢失双向检查完成前到达的提名。现在被控端可在保留现有 DTLS/SRTP/SCTP 会话的情况下跟随控制端换路。

换路接受传统 `USE-CANDIDATE` 及 libwebrtc 的 UInt32 `NOMINATION` (`0xC001`)。带序号提名必须递增；零值、长度错误、旧序号、被更新提名替代的检查响应均不能回退路由。新路径尚未验证时继续使用原路径，待匹配的 triggered check 成功后再选中。相同路径重复提名不重复通知。候选接收任务间用 mutex 串行更新，ICE restart 清空本代状态。原有 STUN 用户名、完整性、角色检查和响应事务/源地址检查保留。

`vendor/webrtc/src/peer_connection/sdp/mod.rs` 声明 `trickle renomination` 接收能力。`src/webrtc_lifecycle_tests.rs` 用现有本地虚拟网络/SCTP 生命周期测试检查该声明，并验证协商仍可建连。

控制端决定弱网切换时机，本补丁没有自行猜测丢包阈值或向云端发送未证实的新信令。初次 P2P 不通时，原有 ICE 检查仍负责选择可用中转；TCP/TLS 中转候选现在默认也可收集。云端必须提供有效 TURN 配置，而且至少一条允许的网络路径可达。

## 分析依据与隔离范围

官方安装目录只读。分析使用 Windows 4.44.0.3919 客户端关键文件的独立副本，没有运行副本、注入现有进程、重启服务、修改配置或防火墙。没有将官方二进制、账号资料、机器地址或凭据加入仓库。

`streamer.dll` SHA-256：`0639a6202b01c73d6f0c6d039cd20760622887f83abcb9c66557e09e395514b6`。
该副本的文件偏移 `0x108ce10` 有诊断字符串 `filter_network_monitor loss rate is too high, try to switch to relay, prefer udp`；偏移 `0xed9d78` 有 `renomination`。这些证明存在相关代码和协议能力，不能单独证明运行时何时触发或网络实测成功。

属性和序号语义对照公开的 libwebrtc 一手源码：[connection.cc](https://github.com/webrtc-mirror/webrtc/blob/main/p2p/base/connection.cc)、[stun.h](https://github.com/webrtc-mirror/webrtc/blob/main/api/transport/stun.h)。实现采用已有 ICE 消息，不使用反编译得到的官方代码。

## 自动验证

本地 Linux 构建、缓存和测试数据位于 `/tmp/uu-relay-test`。运行：

```sh
cargo test --locked
cargo test --manifest-path vendor/webrtc-ice/Cargo.toml --lib relay_switch_tests
```

第二条显式运行依赖库内部测试；根项目 `cargo test` 不会运行依赖库的单元测试。新增测试覆盖：已连接直连到中转、普通检查不抢占、重复提名、检查完成前提名、未知响应、过期 pending 提名、数字提名递增/零值/长度错误/旧序号及 restart 清理。根项目另有无代理 TCP/TLS 配置回归测试。

验证结果（本地 Debian / Rust 1.99.1，2026-11-03）：主机 `cargo test --locked` **280 通过、7 跳过、0 失败**；ICE 库全部单元测试 **102 通过、0 失败**（含新增 5 项）；`cargo test --locked --features native-viewer --bin uuremote-viewer` **175 通过、0 失败**。新增未认证提名回归验证完整性检查仍阻止路由和计数器修改。7 项跳过是项目原有需要专用环境的测试。专用 `.github/workflows/relay-regression.yml` 在修复分支 push 和相关 PR 上执行以上两组测试。

## 用户后续实测（尚未完成）

所有测试副本放在 Linux `/tmp` 下，使用独立构建目录。与已安装被控端同时运行前，须确认独立测试账号/设备及配置，避免两个实例复用同一设备身份。不要直接启动已登录配置的第二个实例。

1. 正常网络连接测试设备，确认桌面、输入、声音及工具连接可用，日志 `ICE path` 显示 `route=direct`。
2. 仅对隔离测试环境的直连媒体路径施加丢包或阻断，保留信令及 TURN 通路。观察官方 Windows 控制端是否重新提名路径，Linux 日志是否变成 `route=relay`，画面和输入是否继续工作，工具连接是否保持。
3. 在仅 TCP/TLS TURN 可达的隔离网络中，不设置 `UU_USE_PROXY`，确认日志显示 stream relays enabled，收集到 relay 候选并建立媒体。日志中的 ICE 候选仍可能标记 UDP：这里的 TCP/TLS 指主机到 TURN 服务器的传输，并非 TURN 服务器到对端的 relay 协议。
4. 恢复网络后重复断开/连接，检查正常 P2P 未退化，并验证桌面、文件、终端各自会话不会相互关闭。

不要使用会输出 token、TURN 凭据、完整 SDP 或用户输入的依赖 TRACE 日志。现有路径诊断只输出协议和候选类型。

自动回归验证状态机和协商，不能替代官方 Windows 控制端加真实云端 TURN 的弱网端到端验收。本补丁目前应视为待实测的兼容性修复。
