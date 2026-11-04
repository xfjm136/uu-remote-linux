use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::{api::setting_engine::SettingEngine, ice::{mdns::MulticastDnsMode, network_type::NetworkType},
    util::vnet::{net::{Net, NetConfig}, router::{Router, RouterConfig}}};

pub(crate) async fn virtual_network() -> (SettingEngine, SettingEngine, Arc<Mutex<Router>>) {
    // Each pair owns its router and address space; no host interfaces or sockets are used.
    let router = Arc::new(Mutex::new(Router::new(RouterConfig {
        cidr: "192.0.2.0/24".into(), ..Default::default()
    }).unwrap()));
    let mut settings = Vec::new();
    for ip in ["192.0.2.1", "192.0.2.2"] {
        let net = Arc::new(Net::new(Some(NetConfig { static_ips: vec![ip.into()], ..Default::default() })));
        let nic = net.get_nic().unwrap();
        router.lock().await.add_net(nic.clone()).await.unwrap();
        nic.lock().await.set_router(router.clone()).await.unwrap();
        let mut setting = SettingEngine::default();
        setting.set_vnet(Some(net));
        setting.set_network_types(vec![NetworkType::Udp4]);
        setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        settings.push(setting);
    }
    router.lock().await.start().await.unwrap();
    let right = settings.pop().unwrap();
    (settings.pop().unwrap(), right, router)
}

pub(crate) struct TurnFixture {
    server: webrtc::turn::server::Server,
    socket: Arc<dyn webrtc::util::Conn + Send + Sync>,
}
impl TurnFixture {
    pub async fn close(self) -> Result<(), webrtc::turn::Error> {
        self.server.close().await?;
        self.socket.close().await.map_err(|e| webrtc::turn::Error::Other(e.to_string()))
    }
}
// turn 0.7.1's allocation.close closes the shared TURN listener. A production
// TURN server keeps its listener alive when one allocation is deleted. Isolate
// that server-library bug in the test fixture; close the actual socket explicitly.
struct SharedListener(Arc<dyn webrtc::util::Conn + Send + Sync>);
#[async_trait::async_trait]
impl webrtc::util::Conn for SharedListener {
    async fn connect(&self, a: std::net::SocketAddr) -> Result<(), webrtc::util::Error> { self.0.connect(a).await }
    async fn recv(&self, b: &mut [u8]) -> Result<usize, webrtc::util::Error> { self.0.recv(b).await }
    async fn recv_from(&self, b: &mut [u8]) -> Result<(usize,std::net::SocketAddr), webrtc::util::Error> { self.0.recv_from(b).await }
    async fn send(&self, b: &[u8]) -> Result<usize, webrtc::util::Error> { self.0.send(b).await }
    async fn send_to(&self, b: &[u8], a: std::net::SocketAddr) -> Result<usize, webrtc::util::Error> { self.0.send_to(b,a).await }
    fn local_addr(&self) -> Result<std::net::SocketAddr,webrtc::util::Error> { self.0.local_addr() }
    fn remote_addr(&self) -> Option<std::net::SocketAddr> { self.0.remote_addr() }
    async fn close(&self) -> Result<(),webrtc::util::Error> { Ok(()) }
}

pub(crate) async fn turn_server(router: &Arc<Mutex<Router>>) -> TurnFixture {
    use webrtc::turn;
    struct Auth;
    impl turn::auth::AuthHandler for Auth {
        fn auth_handle(&self, username: &str, realm: &str, _: std::net::SocketAddr) -> Result<Vec<u8>, turn::Error> {
            if username != "test-user" { return Err(turn::Error::Other("Unknown synthetic user".into())); }
            Ok(turn::auth::generate_auth_key(username, realm, "test-password"))
        }
    }
    let net = Arc::new(Net::new(Some(NetConfig { static_ips:vec!["192.0.2.3".into()], ..Default::default() })));
    let nic = net.get_nic().unwrap();
    router.lock().await.add_net(nic.clone()).await.unwrap();
    nic.lock().await.set_router(router.clone()).await.unwrap();
    let conn = net.bind("192.0.2.3:3478".parse().unwrap()).await.unwrap();
    let server = turn::server::Server::new(turn::server::config::ServerConfig {
        conn_configs:vec![turn::server::config::ConnConfig { conn:Arc::new(SharedListener(conn.clone())),
            relay_addr_generator:Box::new(turn::relay::relay_static::RelayAddressGeneratorStatic {
                relay_address:"192.0.2.3".parse().unwrap(), address:"0.0.0.0".into(), net,
            }) }], realm:"relay-test".into(), auth_handler:Arc::new(Auth),
        channel_bind_timeout:std::time::Duration::ZERO, alloc_close_notify:None,
    }).await.unwrap();
    TurnFixture { server, socket:conn }
}
