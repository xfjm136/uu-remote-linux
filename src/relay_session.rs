use super::*;
use crate::relay::{Monitor, Stage};
use crate::session_ice::SessionIce;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

pub(super) struct RelaySession {
    transport: SessionIce,
    pub monitor: std::sync::Mutex<Monitor>,
    negotiation: Mutex<()>,
    pending: std::sync::atomic::AtomicBool,
    confirm_on_connect: std::sync::atomic::AtomicBool,
}
impl RelaySession {
    pub fn new(transport: SessionIce) -> Self {
        Self { transport, monitor: Default::default(), negotiation: Mutex::new(()), pending: false.into(), confirm_on_connect: false.into() }
    }
}

pub(super) fn start_monitor(relay: &Arc<RelaySession>, pc: &Arc<RTCPeerConnection>, role: &Arc<DesktopRole>,
    context: &Arc<Mutex<SessionContext>>, signal: Arc<crate::signaling::SignalingClient>) {
    let (relay, peer, role, context) = (relay.clone(), Arc::downgrade(pc), role.clone(), context.clone());
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            let Some(pc) = peer.upgrade() else { break; };
            if matches!(pc.connection_state(), RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed) { break; }
            if !role.is_desktop() || !relay.transport.auto_switch.enabled { continue; }
            if pc.connection_state() != RTCPeerConnectionState::Connected {
                relay.monitor.lock().unwrap().reset(); continue;
            }
            if pc.signaling_state() == RTCSignalingState::Stable
                && !relay.pending.load(std::sync::atomic::Ordering::Acquire)
                && relay.confirm_on_connect.swap(false, std::sync::atomic::Ordering::AcqRel) {
                let mut monitor = relay.monitor.lock().unwrap();
                monitor.applied = monitor.attempted;
            }
            if relay.pending.load(std::sync::atomic::Ordering::Acquire) || pc.signaling_state() != RTCSignalingState::Stable { continue; }
            let Some(path) = crate::network_diagnostics::selected_path(&pc).await else { continue; };
            let context = context.lock().await.clone();
            let (Some(client), Some(ice)) = (context.client_id.as_deref(), context.ice_id.as_ref().and_then(serde_json::Value::as_str)) else { continue; };
            let stage = relay.monitor.lock().unwrap().decide(&relay.transport.auto_switch, path.route == "relay");
            let Some(stage) = stage else { continue; };
            if relay.transport.relay_servers(stage).is_empty() {
                log::warn!("Automatic relay switch has no matching cloud TURN servers"); continue;
            }
            let payload = crate::relay::notification(client, ice, stage);
            // Official 9CD8F0 asks the other peer to initiate the restart. Sending an
            // unsolicited restart offer here instead would race the official controller.
            if signal.emit_event(crate::capability::FORWARD_SETTING_EVENT, payload).await.is_ok() {
                log::info!("Automatic relay switch requested: stage={:?}", stage);
            } else { log::warn!("Automatic relay notification failed (details omitted)"); }
        }
    });
}

impl WebRtcSession {
    pub async fn begin_relay_restart(&self, stage: Stage, signal: &crate::signaling::SignalingClient) -> Result<(), String> {
        let _guard = self.relay.negotiation.lock().await;
        if self.pc.signaling_state() != RTCSignalingState::Stable || self.pc.connection_state() != RTCPeerConnectionState::Connected {
            return Err("Connection is not ready for relay restart".into());
        }
        let ctx = self.context.lock().await.clone();
        if ctx.client_id.is_none() || ctx.ice_id.is_none() { return Err("Restart requires established signaling context".into()); }
        let servers = self.relay.transport.relay_servers(stage);
        if servers.is_empty() { return Err("Cloud did not supply the requested relay transport".into()); }
        self.pc.set_restart_ice_servers(servers, true).await.map_err(|_| "Cannot update relay configuration")?;
        let offer = self.pc.create_offer(Some(webrtc::peer_connection::offer_answer_options::RTCOfferOptions {
            ice_restart: true, ..Default::default()
        })).await.map_err(|_| "Cannot create relay restart offer")?;
        self.pc.set_local_description(offer.clone()).await.map_err(|_| "Cannot set relay restart offer")?;
        self.relay.pending.store(true, std::sync::atomic::Ordering::Release);
        self.relay.confirm_on_connect.store(true, std::sync::atomic::Ordering::Release);
        {
            let mut monitor = self.relay.monitor.lock().unwrap();
            monitor.attempted = Some(stage); monitor.reset();
        }
        signal.emit_soac(SoacMessage { msg_type:"restart_ice".into(), sdp:Some(offer.sdp),
            gzip_sdp:None, candidate:None, ice_id:ctx.ice_id, client_id:ctx.client_id,
            app_control_id:ctx.app_control_id, sdp_mid:None, sdp_mline_index:None }).await
            .map_err(|_| "Cannot signal relay restart")?;
        log::info!("Relay restart offer sent: stage={:?}", stage);
        Ok(())
    }

    pub async fn handle_restart(&self, message: &SoacMessage) -> Result<Option<SoacMessage>, String> {
        let _guard = self.relay.negotiation.lock().await;
        if self.pc.signaling_state() == RTCSignalingState::HaveLocalOffer {
            if !self.relay.pending.load(std::sync::atomic::Ordering::Acquire) {
                return Err("Unexpected restart answer".into());
            }
            let mut sdp = message.sdp.clone().unwrap_or_default();
            if sdp.is_empty() {
                if let Some(encoded) = &message.gzip_sdp {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| "Invalid restart answer")?;
                    GzDecoder::new(&bytes[..]).take(1024*1024+1).read_to_string(&mut sdp).map_err(|_| "Invalid restart answer")?;
                }
            }
            if sdp.is_empty() || sdp.len() > 1024*1024 { return Err("Invalid restart answer size".into()); }
            let answer = RTCSessionDescription::answer(sdp).map_err(|_| "Invalid restart answer SDP")?;
            self.pc.set_remote_description(answer).await.map_err(|_| "Cannot apply restart answer")?;
            self.relay.pending.store(false, std::sync::atomic::Ordering::Release);
            Ok(None)
        } else if self.pc.signaling_state() == RTCSignalingState::Stable {
            self.relay.monitor.lock().unwrap().reset();
            let response = self.answer_restart(message).await?;
            self.relay.confirm_on_connect.store(true, std::sync::atomic::Ordering::Release);
            Ok(Some(response))
        } else { Err("Restart negotiation already in progress".into()) }
    }
}
