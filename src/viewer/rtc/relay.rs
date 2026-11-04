//! Linux controller side of official switch_network_notify / restart_ice.
use super::*;
use crate::relay::{Config, Stage};
use webrtc::peer_connection::{signaling_state::RTCSignalingState, peer_connection_state::RTCPeerConnectionState};

impl Peer {
    pub fn configure_relay(&mut self, ack: &Value, desktop: bool) {
        self.relay_policy = Config::from_header(ack);
        self.relay_policy.enabled &= desktop;
        log::info!("Viewer automatic relay: enabled={} loss_percent={} latency_ms={}",self.relay_policy.enabled,self.relay_policy.loss_percent,self.relay_policy.latency_ms);
    }
    pub async fn switch_notification(&mut self, event: &Value) -> Result<Option<Value>, String> {
        let Some((ice, stage)) = crate::relay::parse_notification(event) else { return Ok(None); };
        if ice != self.ice_id || !self.matches_route(event) { return Ok(None); }
        self.restart_relay(stage).await.map(Some)
    }
    pub async fn restart_relay(&mut self, stage: Stage) -> Result<Value,String> {
        if self.restart_pending || self.pc.signaling_state()!=RTCSignalingState::Stable
            || self.pc.connection_state()!=RTCPeerConnectionState::Connected { return Err("Viewer is not ready for relay restart".into()); }
        let servers:Vec<_>=self.relay_servers.iter().filter_map(|s|{
            let urls:Vec<_>=s.urls.iter().filter(|u|match stage {
                Stage::Udp=>u.starts_with("turn:")||u.starts_with("turns:"),
                Stage::Tls=>u.starts_with("turns:"),
            }).cloned().collect();
            (!urls.is_empty()).then(||RTCIceServer{urls,..s.clone()})
        }).collect();
        if servers.is_empty(){return Err("Cloud did not supply the requested TURN transport".into());}
        self.pc.set_restart_ice_servers(servers,true).await.map_err(|_|"Cannot update viewer relay configuration")?;
        let offer=self.pc.create_offer(Some(webrtc::peer_connection::offer_answer_options::RTCOfferOptions{ice_restart:true,..Default::default()})).await.map_err(|_|"Cannot create viewer restart offer")?;
        let mut sdp=offer.sdp.clone();
        // Preserve the same sent-only capability marker as the initial viewer offer.
        if !sdp.contains("a=x-uuremote-quic") {
            if let Some(index)=sdp.find("a=group:BUNDLE") { sdp.insert_str(index,"a=x-uuremote-quic\r\n"); }
        }
        let sdp=super::with_max_message_size(&sdp);
        self.pc.set_local_description(offer).await.map_err(|_|"Cannot set viewer restart offer")?;
        self.restart_pending=true; self.answered=false; self.early.clear();
        self.relay_monitor.attempted=Some(stage); self.relay_monitor.reset();
        log::info!("Viewer relay restart offer: stage={:?}",stage);
        Ok(json!({"client_id":self.remote_client,"data":{"type":"restart_ice","sdp":sdp,"ice_id":self.ice_id,"app_control_id":self.app_id,"ice_network_type":3}}))
    }
    pub(super) async fn receive_restart(&mut self, body:&Value)->Result<(),String>{
        let sdp=super::answer_sdp(body)?;
        if self.restart_pending && self.pc.signaling_state()==RTCSignalingState::HaveLocalOffer {
            self.pc.set_remote_description(RTCSessionDescription::answer(sdp).map_err(|_|"Invalid viewer restart answer")?).await.map_err(|_|"Cannot apply viewer restart answer")?;
            self.restart_pending=false; self.answered=true;
            for candidate in self.early.drain(..){self.pc.add_ice_candidate(candidate).await.map_err(|_|"Restart ICE candidate failed")?;}
            self.relay_monitor.applied=self.relay_monitor.attempted;
        } else if self.pc.signaling_state()==RTCSignalingState::Stable {
            self.pc.set_remote_description(RTCSessionDescription::offer(sdp).map_err(|_|"Invalid host restart offer")?).await.map_err(|_|"Cannot apply host restart offer")?;
            let answer=self.pc.create_answer(None).await.map_err(|_|"Cannot create viewer restart answer")?;
            let sdp=super::with_max_message_size(&answer.sdp);
            self.pc.set_local_description(answer).await.map_err(|_|"Cannot set viewer restart answer")?;
            self.signal_outbound.send(json!({"client_id":self.remote_client,"data":{"type":"restart_ice","sdp":sdp,"ice_id":self.ice_id,"app_control_id":self.app_id}})).await.map_err(|_|"Viewer restart signaling closed")?;
            self.answered=true; self.relay_monitor.reset();
        } else {return Err("Conflicting viewer restart negotiation".into());}
        Ok(())
    }
    pub async fn quality_notification(&mut self)->Option<Value>{
        if !self.relay_policy.enabled || self.restart_pending || self.pc.connection_state()!=RTCPeerConnectionState::Connected
            || self.pc.signaling_state()!=RTCSignalingState::Stable {return None;}
        let expected=self.observed.video_expected.load(Ordering::Relaxed);
        let received=self.observed.video_received.load(Ordering::Relaxed);
        let (e,r)=(expected.saturating_sub(self.relay_previous.0),received.saturating_sub(self.relay_previous.1));
        self.relay_previous=(expected,received);
        if e==0{return None;}
        let (rtt,route)=self.route().await;
        self.relay_monitor.observe(self.desktop.ssrc.load(Ordering::Relaxed),e.saturating_sub(r) as f64/e as f64,rtt.map(f64::from));
        let stage=self.relay_monitor.decide(&self.relay_policy,route.contains("relay"))?;
        Some(crate::relay::notification(&self.remote_client,&self.ice_id,stage))
    }
}
