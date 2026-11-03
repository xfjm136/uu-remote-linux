use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use stun::agent::*;
use stun::attributes::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use tokio::time::{Duration, Instant};

use crate::agent::agent_internal::*;
use crate::candidate::*;
use crate::control::*;
use crate::priority::*;
use crate::use_candidate::*;

// libwebrtc's STUN_ATTR_NOMINATION (api/transport/stun.h, connection.cc).
const ATTR_NOMINATION: AttrType = AttrType(0xC001);

#[async_trait]
trait ControllingSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
}

#[async_trait]
trait ControlledSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
}

impl AgentInternal {
    // UU's Windows streamer reports "loss rate is too high, try to switch to relay,
    // prefer udp" (4.44.0.3919). Its controlling ICE agent must be able to nominate
    // another valid pair without replacing the DTLS/SCTP session. Incoming requests
    // reach this selector only after handle_inbound verifies STUN integrity/role.
    async fn select_remote_nomination(&self, pair: &Arc<CandidatePair>) {
        let selected = self.agent_conn.get_selected_pair();
        if selected.as_ref().map_or(true, |old| old.as_ref() != pair.as_ref()) {
            self.set_selected_pair(Some(Arc::clone(pair))).await;
        }
    }

    fn is_nominatable(&self, c: &Arc<dyn Candidate + Send + Sync>) -> bool {
        let start_time = *self.start_time.lock();
        match c.candidate_type() {
            CandidateType::Host => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.host_acceptance_min_wait.as_nanos()
            }
            CandidateType::ServerReflexive => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.srflx_acceptance_min_wait.as_nanos()
            }
            CandidateType::PeerReflexive => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.prflx_acceptance_min_wait.as_nanos()
            }
            CandidateType::Relay => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.relay_acceptance_min_wait.as_nanos()
            }
            CandidateType::Unspecified => {
                log::error!(
                    "is_nominatable invalid candidate type {}",
                    c.candidate_type()
                );
                false
            }
        }
    }

    async fn nominate_pair(&self) {
        let result = {
            let nominated_pair = self.nominated_pair.lock().await;
            if let Some(pair) = &*nominated_pair {
                // The controlling agent MUST include the USE-CANDIDATE attribute in
                // order to nominate a candidate pair (Section 8.1.1).  The controlled
                // agent MUST NOT include the USE-CANDIDATE attribute in a Binding
                // request.

                let (msg, result) = {
                    let ufrag_pwd = self.ufrag_pwd.lock().await;
                    let username =
                        ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
                    let mut msg = Message::new();
                    let result = msg.build(&[
                        Box::new(BINDING_REQUEST),
                        Box::new(TransactionId::new()),
                        Box::new(Username::new(ATTR_USERNAME, username)),
                        Box::<UseCandidateAttr>::default(),
                        Box::new(AttrControlling(self.tie_breaker.load(Ordering::SeqCst))),
                        Box::new(PriorityAttr(pair.local.priority())),
                        Box::new(MessageIntegrity::new_short_term_integrity(
                            ufrag_pwd.remote_pwd.clone(),
                        )),
                        Box::new(FINGERPRINT),
                    ]);
                    (msg, result)
                };

                if let Err(err) = result {
                    log::error!("{}", err);
                    None
                } else {
                    log::trace!(
                        "ping STUN (nominate candidate pair from {} to {}",
                        pair.local,
                        pair.remote
                    );
                    let local = pair.local.clone();
                    let remote = pair.remote.clone();
                    Some((msg, local, remote))
                }
            } else {
                None
            }
        };

        if let Some((msg, local, remote)) = result {
            self.send_binding_request(&msg, &local, &remote).await;
        }
    }

    pub(crate) async fn start(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::start(self).await;
        } else {
            ControlledSelector::start(self).await;
        }
    }

    pub(crate) async fn contact_candidates(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::contact_candidates(self).await;
        } else {
            ControlledSelector::contact_candidates(self).await;
        }
    }

    pub(crate) async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::ping_candidate(self, local, remote).await;
        } else {
            ControlledSelector::ping_candidate(self, local, remote).await;
        }
    }

    pub(crate) async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        } else {
            ControlledSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        }
    }

    pub(crate) async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::handle_binding_request(self, m, local, remote).await;
        } else {
            ControlledSelector::handle_binding_request(self, m, local, remote).await;
        }
    }
}

#[async_trait]
impl ControllingSelector for AgentInternal {
    async fn start(&self) {
        {
            let mut nominated_pair = self.nominated_pair.lock().await;
            *nominated_pair = None;
        }
        *self.start_time.lock() = Instant::now();
    }

    async fn contact_candidates(&self) {
        // A lite selector should not contact candidates
        if self.lite.load(Ordering::SeqCst) {
            // This only happens if both peers are lite. See RFC 8445 S6.1.1 and S6.2
            log::trace!("now falling back to full agent");
        }

        let nominated_pair_is_some = {
            let nominated_pair = self.nominated_pair.lock().await;
            nominated_pair.is_some()
        };

        if self.agent_conn.get_selected_pair().is_some() {
            if self.validate_selected_pair().await {
                log::trace!("[{}]: checking keepalive", self.get_name());
                self.check_keepalive().await;
            }
        } else if nominated_pair_is_some {
            self.nominate_pair().await;
        } else {
            let has_nominated_pair =
                if let Some(p) = self.agent_conn.get_best_valid_candidate_pair().await {
                    self.is_nominatable(&p.local) && self.is_nominatable(&p.remote)
                } else {
                    false
                };

            if has_nominated_pair {
                if let Some(p) = self.agent_conn.get_best_valid_candidate_pair().await {
                    log::trace!(
                        "Nominatable pair found, nominating ({}, {})",
                        p.local.to_string(),
                        p.remote.to_string()
                    );
                    p.nominated.store(true, Ordering::SeqCst);
                    {
                        let mut nominated_pair = self.nominated_pair.lock().await;
                        *nominated_pair = Some(p);
                    }
                }

                self.nominate_pair().await;
            } else {
                self.ping_all_candidates().await;
            }
        }
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let (msg, result) = {
            let ufrag_pwd = self.ufrag_pwd.lock().await;
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let result = msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlling(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(local.priority())),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (msg, result)
        };

        if let Err(err) = result {
            log::error!("{}", err);
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if let Some(pending_request) = self.handle_inbound_binding_success(m.transaction_id).await {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({}), actual({})", transaction_addr, remote);
                return;
            }

            log::trace!(
                "inbound STUN (SuccessResponse) from {} to {}",
                remote,
                local
            );
            if let Some(p) = self.find_pair(local, remote).await {
                p.record_rtt(pending_request.timestamp.elapsed());
            }
            let selected_pair_is_none = self.agent_conn.get_selected_pair().is_none();

            if let Some(p) = self.find_pair(local, remote).await {
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                log::trace!(
                    "Found valid candidate pair: {}, p.state: {}, isUseCandidate: {}, {}",
                    p,
                    p.state.load(Ordering::SeqCst),
                    pending_request.is_use_candidate,
                    selected_pair_is_none
                );
                if pending_request.is_use_candidate && selected_pair_is_none {
                    self.set_selected_pair(Some(Arc::clone(&p))).await;
                }
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }

    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        self.send_binding_success(m, local, remote).await;
        log::trace!("controllingSelector: sendBindingSuccess");

        if let Some(p) = self.find_pair(local, remote).await {
            let nominated_pair_is_none = {
                let nominated_pair = self.nominated_pair.lock().await;
                nominated_pair.is_none()
            };

            log::trace!(
                "controllingSelector: after findPair {}, p.state: {}, {}",
                p,
                p.state.load(Ordering::SeqCst),
                nominated_pair_is_none,
                //self.agent_conn.get_selected_pair().await.is_none() //, {}
            );
            if p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8
                && nominated_pair_is_none
                && self.agent_conn.get_selected_pair().is_none()
            {
                if let Some(best_pair) = self.agent_conn.get_best_available_candidate_pair().await {
                    log::trace!(
                        "controllingSelector: getBestAvailableCandidatePair {}",
                        best_pair
                    );
                    if best_pair == p
                        && self.is_nominatable(&p.local)
                        && self.is_nominatable(&p.remote)
                    {
                        log::trace!("The candidate ({}, {}) is the best candidate available, marking it as nominated",
                            p.local, p.remote);
                        {
                            let mut nominated_pair = self.nominated_pair.lock().await;
                            *nominated_pair = Some(p);
                        }
                        self.nominate_pair().await;
                    }
                } else {
                    log::trace!("No best pair available");
                }
            }
        } else {
            log::trace!("controllingSelector: addPair");
            self.add_pair(local.clone(), remote.clone()).await;
        }
    }
}

#[async_trait]
impl ControlledSelector for AgentInternal {
    async fn start(&self) {
        *self.nominated_pair.lock().await = None;
        self.remote_nomination.store(0, Ordering::SeqCst);
    }

    async fn contact_candidates(&self) {
        // A lite selector should not contact candidates
        if self.lite.load(Ordering::SeqCst) {
            self.validate_selected_pair().await;
        } else if self.agent_conn.get_selected_pair().is_some() {
            if self.validate_selected_pair().await {
                log::trace!("[{}]: checking keepalive", self.get_name());
                self.check_keepalive().await;
            }
        } else {
            self.ping_all_candidates().await;
        }
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let (msg, result) = {
            let ufrag_pwd = self.ufrag_pwd.lock().await;
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let result = msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlled(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(local.priority())),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (msg, result)
        };

        if let Err(err) = result {
            log::error!("{}", err);
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        // https://tools.ietf.org/html/rfc8445#section-7.3.1.5
        // If the controlled agent does not accept the request from the
        // controlling agent, the controlled agent MUST reject the nomination
        // request with an appropriate error code response (e.g., 400)
        // [RFC5389].

        if let Some(pending_request) = self.handle_inbound_binding_success(m.transaction_id).await {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({}), actual({})", transaction_addr, remote);
                return;
            }

            log::trace!(
                "inbound STUN (SuccessResponse) from {} to {}",
                remote,
                local
            );
            if let Some(p) = self.find_pair(local, remote).await {
                p.record_rtt(pending_request.timestamp.elapsed());
            }

            if let Some(p) = self.find_pair(local, remote).await {
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                log::trace!("Found valid candidate pair: {}", p);
                // A nomination can arrive before our triggered connectivity check
                // succeeds. Only the latest nominated pair may take over the path.
                let nominated = self.nominated_pair.lock().await;
                if nominated.as_ref().is_some_and(|n| n.as_ref() == p.as_ref()) {
                    self.select_remote_nomination(&p).await;
                }
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }

    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.find_pair(local, remote).await.is_none() {
            self.add_pair(local.clone(), remote.clone()).await;
        }

        if let Some(p) = self.find_pair(local, remote).await {
            // Serialize nomination order across candidate receive loops, including
            // selection, so a late check response cannot win over a newer request.
            let mut nominated_pair = self.nominated_pair.lock().await;
            // libwebrtc uses either USE-CANDIDATE or a monotonically increasing
            // NOMINATION, never both. Stale/invalid counters cannot revert a route.
            let nominate = if m.contains(ATTR_NOMINATION) {
                match m.get(ATTR_NOMINATION) {
                    Ok(v) if v.len() == 4 => {
                        let value = u32::from_be_bytes([v[0], v[1], v[2], v[3]]);
                        let previous = self.remote_nomination.load(Ordering::SeqCst);
                        if value > previous {
                            self.remote_nomination.store(value, Ordering::SeqCst);
                            true
                        } else { false }
                    }
                    _ => false,
                }
            } else {
                // Legacy requests have no ordering counter. A retransmission from
                // a formerly selected pair must not undo a later route change.
                let selected = self.agent_conn.get_selected_pair();
                m.contains(ATTR_USE_CANDIDATE) && self.remote_nomination.load(Ordering::SeqCst) == 0
                    && (!p.nominated.load(Ordering::SeqCst)
                        || selected.as_ref().is_some_and(|old| old.as_ref() == p.as_ref()))
            };
            if nominate {
                *nominated_pair = Some(Arc::clone(&p));
                // https://tools.ietf.org/html/rfc8445#section-7.3.1.5

                if p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8 {
                    // If the state of this pair is Succeeded, it means that the check
                    // previously sent by this pair produced a successful response and
                    // generated a valid pair (Section 7.2.5.3.2).  The agent sets the
                    // nominated flag value of the valid pair to true.
                    self.select_remote_nomination(&p).await;
                    drop(nominated_pair);
                    self.send_binding_success(m, local, remote).await;
                } else {
                    // If the received Binding request triggered a new check to be
                    // enqueued in the triggered-check queue (Section 7.3.1.4), once the
                    // check is sent and if it generates a successful response, and
                    // generates a valid pair, the agent sets the nominated flag of the
                    // pair to true.  If the request fails (Section 7.2.5.2), the agent
                    // MUST remove the candidate pair from the valid list, set the
                    // candidate pair state to Failed, and set the checklist state to
                    // Failed.
                    drop(nominated_pair);
                    self.send_binding_success(m, local, remote).await;
                    self.ping_candidate(local, remote).await;
                }
            } else {
                drop(nominated_pair);
                self.send_binding_success(m, local, remote).await;
                self.ping_candidate(local, remote).await;
            }
        }
    }
}

#[cfg(test)]
mod relay_switch_tests {
    use super::*;
    use crate::agent::{Agent, agent_config::AgentConfig, agent_vnet_test::MockConn};
    use crate::candidate::candidate_base::CandidateBaseConfig;
    use crate::candidate::candidate_host::CandidateHostConfig;
    use crate::candidate::candidate_relay::CandidateRelayConfig;

    async fn fixture() -> (Agent, Arc<CandidatePair>, Arc<CandidatePair>) {
        let agent = Agent::new(AgentConfig::default()).await.unwrap();
        let host = |port| -> Arc<dyn Candidate + Send + Sync> {
            Arc::new(CandidateHostConfig { base_config: CandidateBaseConfig {
                network: "udp".into(), address: "192.0.2.1".into(), port, component: 1,
                conn: Some(Arc::new(MockConn)), ..Default::default()
            }, ..Default::default() }.new_candidate_host().unwrap())
        };
        let local = host(1000);
        let direct = host(2000);
        let relay: Arc<dyn Candidate + Send + Sync> = Arc::new(CandidateRelayConfig {
            base_config: CandidateBaseConfig { network: "udp".into(), address: "192.0.2.2".into(),
                port: 3000, component: 1, ..Default::default() },
            ..Default::default()
        }.new_candidate_relay().unwrap());
        agent.internal.add_pair(local.clone(), direct.clone()).await;
        agent.internal.add_pair(local.clone(), relay.clone()).await;
        let direct = agent.internal.find_pair(&local, &direct).await.unwrap();
        let relay = agent.internal.find_pair(&local, &relay).await.unwrap();
        direct.state.store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
        relay.state.store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
        agent.internal.set_selected_pair(Some(direct.clone())).await;
        (agent, direct, relay)
    }

    fn request(nominate: bool) -> Message {
        let mut msg = Message::new();
        msg.build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new())]).unwrap();
        if nominate { UseCandidateAttr::new().add_to(&mut msg).unwrap(); }
        msg
    }

    #[tokio::test]
    async fn controlled_peer_switches_from_direct_to_nominated_relay() {
        let (a, direct, relay) = fixture().await;
        // A successful ordinary check must not steal the selected route.
        ControlledSelector::handle_binding_request(&*a.internal, &request(false), &relay.local, &relay.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &relay.local, &relay.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), relay);
        // Duplicate nomination retains the same pair (and the same agent/session).
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &relay.local, &relay.remote).await;
        assert!(Arc::ptr_eq(&a.internal.agent_conn.get_selected_pair().unwrap(), &relay));
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &direct.local, &direct.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), relay);
        a.close().await.unwrap();
    }

    #[tokio::test]
    async fn nomination_waits_for_triggered_check_and_ignores_unknown_response() {
        let (a, direct, relay) = fixture().await;
        relay.state.store(CandidatePairState::Waiting as u8, Ordering::SeqCst);
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &relay.local, &relay.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        let mut response = Message::new();
        response.build(&[Box::new(BINDING_SUCCESS), Box::new(TransactionId::new())]).unwrap();
        ControlledSelector::handle_success_response(&*a.internal, &response, &relay.local, &relay.remote, relay.remote.addr()).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        response.transaction_id = a.internal.pending_binding_requests.lock().await.last().unwrap().transaction_id;
        ControlledSelector::handle_success_response(&*a.internal, &response, &relay.local, &relay.remote, relay.remote.addr()).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), relay);
        a.close().await.unwrap();
    }

    #[tokio::test]
    async fn superseded_pending_nomination_does_not_revert_the_route() {
        let (a, direct, relay) = fixture().await;
        relay.state.store(CandidatePairState::Waiting as u8, Ordering::SeqCst);
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &relay.local, &relay.remote).await;
        let tid = a.internal.pending_binding_requests.lock().await.last().unwrap().transaction_id;
        ControlledSelector::handle_binding_request(&*a.internal, &request(true), &direct.local, &direct.remote).await;
        let mut response = Message::new();
        response.build(&[Box::new(BINDING_SUCCESS), Box::new(tid)]).unwrap();
        ControlledSelector::handle_success_response(&*a.internal, &response, &relay.local, &relay.remote, relay.remote.addr()).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        a.close().await.unwrap();
    }

    #[tokio::test]
    async fn numeric_renomination_rejects_stale_zero_and_malformed_counters() {
        let (a, direct, relay) = fixture().await;
        let numeric = |value: &[u8]| {
            let mut msg = request(false);
            msg.add(ATTR_NOMINATION, value);
            msg
        };
        for value in [vec![0, 0, 0], 0u32.to_be_bytes().to_vec()] {
            ControlledSelector::handle_binding_request(&*a.internal, &numeric(&value), &relay.local, &relay.remote).await;
            assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        }
        ControlledSelector::handle_binding_request(&*a.internal, &numeric(&2u32.to_be_bytes()), &relay.local, &relay.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), relay);
        // Delayed old nominations and legacy keepalives must not switch back.
        for msg in [numeric(&1u32.to_be_bytes()), numeric(&2u32.to_be_bytes()), request(true)] {
            ControlledSelector::handle_binding_request(&*a.internal, &msg, &direct.local, &direct.remote).await;
            assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), relay);
        }
        ControlledSelector::handle_binding_request(&*a.internal, &numeric(&3u32.to_be_bytes()), &direct.local, &direct.remote).await;
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        // ICE restart starts a new nomination generation.
        ControlledSelector::start(&*a.internal).await;
        assert_eq!(a.internal.remote_nomination.load(Ordering::SeqCst), 0);
        assert!(a.internal.nominated_pair.lock().await.is_none());
        a.close().await.unwrap();
    }

    #[tokio::test]
    async fn unauthenticated_nomination_cannot_change_route_or_counter() {
        let (a, direct, relay) = fixture().await;
        let username = {
            let credentials = a.internal.ufrag_pwd.lock().await;
            format!("{}:{}", credentials.local_ufrag, credentials.remote_ufrag)
        };
        let mut msg = Message::new();
        msg.build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new()),
            Box::new(Username::new(ATTR_USERNAME, username)),
            Box::new(AttrControlling(123)), Box::new(PriorityAttr(relay.remote.priority()))]).unwrap();
        msg.add(ATTR_NOMINATION, &2u32.to_be_bytes());
        MessageIntegrity::new_short_term_integrity("invalid-test-key".into()).add_to(&mut msg).unwrap();
        FINGERPRINT.add_to(&mut msg).unwrap();
        a.internal.handle_inbound(&mut msg, &relay.local, relay.remote.addr()).await;
        assert_eq!(a.internal.remote_nomination.load(Ordering::SeqCst), 0);
        assert_eq!(a.internal.agent_conn.get_selected_pair().unwrap(), direct);
        a.close().await.unwrap();
    }
}
