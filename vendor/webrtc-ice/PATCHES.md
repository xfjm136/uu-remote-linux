# Local patch to webrtc-ice 0.10.1

Source: crates.io webrtc-ice 0.10.1 (MIT / Apache-2.0; upstream licenses retained).

Upstream gathers relay candidates only for `turn:` over UDP and logs "Unable to handle URL" for
`turn:?transport=tcp` and `turns:`. NetEase sessions that force relaying also offer
`turns:<host>:<port>?transport=tcp`, which is the only media path on hosts whose UDP cannot reach
the TURN servers but whose TCP goes through an HTTP proxy.

`src/agent/turn_tcp.rs` opens TCP (directly, or through an HTTP CONNECT proxy set by the
application with `set_relay_transport`), adds TLS for `turns:`, and exposes the stream to the
unchanged `turn` client as datagrams: one STUN message or ChannelData frame (with its 4-byte TCP
padding) per read. `agent_gather.rs` uses it for `ProtoType::Tcp` URLs. UDP gathering is unchanged.

TLS trusts the system certificate store (`rustls-native-certs`, as OpenSSL does) plus the bundled
Mozilla roots. A name mismatch is accepted only when the TURN URL is an IP literal.

The application now enables direct TCP/TLS gathering by default. `UU_USE_PROXY` selects
whether an environment HTTP proxy is used; it no longer gates stream relay availability.

## Controlled-side path re-nomination

The official Windows 4.44.0.3919 `streamer.dll` contains the diagnostic
`filter_network_monitor loss rate is too high, try to switch to relay, prefer udp` and
the ICE option `renomination`. Upstream's controlled selector only selected a pair
while no pair was selected, preventing an established direct path from following a
new nomination. It also forgot nominations received before a triggered check succeeded.

The controlled selector now follows authenticated `USE-CANDIDATE` requests for valid
pairs and libwebrtc's increasing UInt32 `NOMINATION` attribute (`0xC001`). The latest
pending nomination is retained until its triggered check succeeds. Numeric stale, zero,
malformed, and superseded nominations cannot replace the selected route; duplicates
do not emit a pair-change notification. A mutex serializes selection across candidate
receive loops. Counters and pending state reset with ICE restart.

`vendor/webrtc` advertises `a=ice-options:trickle renomination` for this receive capability.
Selection updates the existing `AgentConn`; DTLS/SRTP/SCTP are not recreated. The official
controller still decides when and which route to nominate. This patch does not add an
independent host-side loss threshold or change the controlling selector.

Protocol references: libwebrtc [connection.cc](https://github.com/webrtc-mirror/webrtc/blob/main/p2p/base/connection.cc)
(`HandleBindingRequest`, request construction) and [stun.h](https://github.com/webrtc-mirror/webrtc/blob/main/api/transport/stun.h).
See [relay acceptance notes](../../docs/relay-fallback.md) for verification limits.

## Round-trip time

Upstream never measures it: `CandidatePairStats::current_round_trip_time` is always 0. A successful
binding response now records `now - request timestamp` on its candidate pair, and the stats report
the latest sample. Keepalive pings only an idle pair, which a streaming session never is, so the
selected pair is also pinged when its last sample is older than the keepalive interval (a binding
request, like a consent check from RFC 7675).

The host (`uuremote-server`) uses the same crate, so on the selected pair it also sends one
binding request per keepalive interval (2 s) while media flows. Checked on the host side with the
official Android client controlling a Linux host (GNOME, cloud ICE servers incl. TURN) over repeated
sessions: the client answers every request and sessions stay up. Official Windows/macOS controllers
have not been tried against the host yet.

## Per-generation relay configuration

`Agent::set_restart_options` stages validated server URLs and candidate types for the next ICE gather. It preserves the AgentConn used by DTLS/SCTP and rejects updates during active gathering. The application retains cloud credentials per peer. The WebRTC wrapper exposes `set_restart_ice_servers`; this does not replace the peer or update unrelated sessions. See docs/relay-fallback.md for official notification evidence and verification limits.
