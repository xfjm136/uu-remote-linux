# 会话内中转切换修复 / Relay fallback

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
