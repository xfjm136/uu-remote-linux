//! Official 4.44 streamer: AED920 (decision), 9CD8F0 (1 Hz notification),
//! 9CFCF0 (wire fields). No credentials or account data in diagnostics.
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage { Udp, Tls }
impl Stage { pub fn wire(self) -> u64 { match self { Self::Udp => 1, Self::Tls => 2 } } }

#[derive(Clone, Default)]
pub struct Config { pub enabled: bool, pub loss_percent: u32, pub latency_ms: u32 }
impl Config {
    pub fn from_header(header: &Value) -> Self {
        let data = header.get("data").unwrap_or(header);
        let field = |key| header.get(key).or_else(|| data.get(key));
        let number = |key| field(key).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let loss_percent = number("force_auto_switch_pkt_loss");
        let latency_ms = number("force_auto_switch_latency");
        Self { enabled: field("auto_switch_network").and_then(Value::as_bool).unwrap_or(false)
            && loss_percent >= 10 && latency_ms >= 200,
            loss_percent, latency_ms }
    }
}

#[derive(Default)]
struct Window { values: VecDeque<f64> }
impl Window {
    fn push(&mut self, value: f64) { self.values.push_back(value); if self.values.len() > 15 { self.values.pop_front(); } }
    fn mean(&self) -> Option<f64> { (self.values.len() >= 15).then(|| self.values.iter().sum::<f64>() / self.values.len() as f64) }
}
#[derive(Default)]
struct Stream { loss: Window, latency: Window, latest: Option<(f64, Option<f64>)> }
#[derive(Default)]
pub struct Monitor { streams: HashMap<u32, Stream>, pub attempted: Option<Stage>, pub applied: Option<Stage> }
impl Monitor {
    pub fn observe(&mut self, ssrc: u32, loss: f64, latency: Option<f64>) {
        if !loss.is_finite() || !(0.0..=1.0).contains(&loss) { return; }
        if self.streams.len() >= 32 && !self.streams.contains_key(&ssrc) { return; }
        let stream = self.streams.entry(ssrc).or_default();
        stream.latest = Some((loss, latency.filter(|v| v.is_finite() && *v >= 0.0 && *v < 30_000.0)));
    }
    pub fn decide(&mut self, config: &Config, relayed: bool) -> Option<Stage> {
        if !config.enabled || (relayed && self.applied == Some(Stage::Tls)) { return None; }
        let mut bad = false;
        for stream in self.streams.values_mut() {
            let Some((loss, latency)) = stream.latest.take() else { continue; };
            stream.loss.push(loss);
            if let Some(ms) = latency { stream.latency.push(ms); }
            bad |= stream.loss.mean().is_some_and(|v| (v * 100.0) as u32 > config.loss_percent)
                || stream.latency.mean().is_some_and(|v| v > f64::from(config.latency_ms));
        }
        if !bad { return None; }
        let stage = if relayed || self.attempted == Some(Stage::Tls) { Stage::Tls } else { Stage::Udp };
        self.attempted = Some(stage);
        self.streams.clear();
        Some(stage)
    }
    pub fn reset(&mut self) { self.streams.clear(); }
}

pub fn notification(client: &str, ice: &str, stage: Stage) -> Value {
    json!({"client_id":client,"data":{"type":"switch_network_notify",
        "switch_network_notify":{"transport_type":1,"ice_id":ice,"attempt_switch_type":stage.wire()}}})
}
pub fn parse_notification(event: &Value) -> Option<(String, Stage)> {
    let data = event.get("data").unwrap_or(event);
    if data.get("type")?.as_str()? != "switch_network_notify" { return None; }
    let body = data.get("switch_network_notify")?;
    if body.get("transport_type")?.as_u64()? != 1 { return None; }
    let stage = match body.get("attempt_switch_type")?.as_u64()? { 1 => Stage::Udp, 2 => Stage::Tls, _ => return None };
    let ice = body.get("ice_id")?.as_str()?;
    if ice.is_empty() || ice.len() > 256 { return None; }
    Some((ice.to_owned(), stage))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_parameters_gate_switching_and_boundaries_match_official() {
        let config = Config::from_header(&json!({"auto_switch_network":true,"force_auto_switch_pkt_loss":10,"force_auto_switch_latency":200}));
        assert!(config.enabled);
        let mut monitor = Monitor::default();
        for _ in 0..14 { monitor.observe(1, 0.11, Some(200.)); assert_eq!(monitor.decide(&config, false), None); }
        assert_eq!(monitor.decide(&config, false), None);
        monitor.observe(1, 0.11, Some(200.));
        assert_eq!(monitor.decide(&config, false), Some(Stage::Udp));
        assert_eq!(monitor.decide(&config, false), None);
        for _ in 0..15 { monitor.observe(1, 0.10, Some(200.)); assert_eq!(monitor.decide(&config, true), None); }
        assert_eq!(monitor.decide(&config, true), None);
        monitor.reset();
        for _ in 0..14 { monitor.observe(1, 0.25, Some(201.)); assert_eq!(monitor.decide(&config, true), None); }
        monitor.observe(1, 0.25, Some(201.));
        assert_eq!(monitor.decide(&config, true), Some(Stage::Tls));
        monitor.applied = Some(Stage::Tls);
        for _ in 0..15 { monitor.observe(1, 0.90, Some(2000.)); }
        assert_eq!(monitor.decide(&config, true), None);
        assert!(!Config::from_header(&json!({"auto_switch_network":true,"force_auto_switch_pkt_loss":9,"force_auto_switch_latency":200})).enabled);
        assert!(!Config::from_header(&json!({})).enabled);
    }
    #[test]
    fn windows_are_per_stream_and_notifications_match_verified_schema() {
        let config = Config { enabled:true, loss_percent:10, latency_ms:200 };
        let mut monitor = Monitor::default();
        for _ in 0..8 { monitor.observe(1, 0.5, None); monitor.observe(2, 0.5, None); }
        assert_eq!(monitor.decide(&config, false), None);
        for stage in [Stage::Udp, Stage::Tls] { assert_eq!(parse_notification(&notification("test", "ice", stage)), Some(("ice".into(), stage))); }
        assert!(parse_notification(&json!({"data":{"type":"switch_network_notify","switch_network_notify":{"transport_type":1,"ice_id":"ice","attempt_switch_type":99}}})).is_none());
    }
}
