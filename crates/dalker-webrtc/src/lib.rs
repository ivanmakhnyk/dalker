//! # dalker-webrtc
//!
//! Turns a stream of [`RawFrame`]s into a WebRTC H.264 video track and turns the
//! viewer's `"control"` DataChannel messages into [`InputEvent`]s.
//!
//! The same channel carries the text clipboard both ways (see [`clipboard`]), the
//! viewer's stream settings (see [`settings`]) and, when the host ends a session, the
//! reason (see [`EndReason`]). File transfers use a second channel,
//! `"files"` (see [`files`]).
//!
//! Signaling is a single non-trickle offer/answer exchange: the browser sends
//! its complete offer, [`WebRtcEngine::accept_offer`] returns a complete answer
//! (all ICE candidates gathered). This keeps the HTTP API trivial and works
//! well on a LAN or through a fixed UDP port forward.

pub mod clipboard;
pub mod encoder;
pub mod files;
pub mod settings;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use dalker_core::{InputEvent, RawFrame};
use tokio::sync::{mpsc, watch};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub use clipboard::{ClipboardState, ClipboardWatch};
pub use encoder::EncoderSettings;
pub use files::{FileAccessFactory, FILES_CHANNEL};

/// Label of the DataChannel that carries input events.
pub const CONTROL_CHANNEL: &str = "control";

pub type SessionId = u64;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Message from a session to the agent's input worker.
#[derive(Debug, Clone)]
pub enum ControlMessage {
    Input(InputEvent),
    /// The viewer copied this text: put it on the host's clipboard. It arrives in order with
    /// the input, so a paste shortcut that follows it pastes this text.
    Clipboard(String),
    /// The peer is gone; held keys/buttons must be released.
    Closed,
    /// Answered once everything sent before it has been applied (an AI agent waits for its
    /// input to land before it looks at the screen).
    Flush(std::sync::mpsc::Sender<()>),
}

/// Why the host ends a session. [`Session::end`] tells the viewer before it closes, so the
/// viewer can say why instead of a bare "disconnected":
///
/// ```text
/// {"type":"session_end","reason":"password_changed"}     host → viewer
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The access password was changed on the host: every viewer is signed out.
    PasswordChanged,
    /// Web remote access was turned off on the host.
    WebOff,
    /// The agent is shutting down (service stop or restart, host shutdown).
    HostShutdown,
    /// The viewer's sign-in was signed out (from another tab, for example).
    SignedOut,
    /// The viewer's sign-in reached its lifetime.
    SessionExpired,
    /// The person at the host chose "Disconnect everyone".
    HostDisconnected,
}

impl EndReason {
    /// The `reason` value of the `session_end` message.
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::PasswordChanged => "password_changed",
            EndReason::WebOff => "web_off",
            EndReason::HostShutdown => "host_shutdown",
            EndReason::SignedOut => "signed_out",
            EndReason::SessionExpired => "session_expired",
            EndReason::HostDisconnected => "host_disconnected",
        }
    }

    /// The `session_end` message sent to the viewer.
    pub fn message(self) -> String {
        serde_json::json!({"type": "session_end", "reason": self.as_str()}).to_string()
    }
}

/// How long [`Session::end`] waits for the `session_end` message to leave before it closes.
const END_FLUSH: Duration = Duration::from_secs(1);

/// Latest captured frame, shared by all sessions. `None` until the first capture.
pub type FrameWatch = watch::Receiver<Option<Arc<RawFrame>>>;

#[derive(Debug, Clone, Default)]
pub struct WebRtcConfig {
    /// STUN/TURN URLs, e.g. `stun:stun.l.google.com:19302`. Empty = LAN only.
    pub ice_servers: Vec<String>,
    /// Serve all ICE traffic from this single UDP port (makes port-forwarding,
    /// firewalls and QEMU `hostfwd=udp:...` possible). `None` = ephemeral ports.
    pub udp_port: Option<u16>,
    /// Extra host candidate IPs to advertise (NAT 1:1), e.g. the address a
    /// viewer reaches a port-forwarded VM on.
    pub public_ips: Vec<String>,
    pub encoder: EncoderSettings,
    /// A viewer that has not reached `Connected` this long after the answer was sent is
    /// dropped, so a browser that can never complete ICE does not hold a viewer slot (and the
    /// on-screen border) until the ICE agent finally gives up. `None` = 20 s.
    pub connect_timeout: Option<Duration>,
    /// How long a viewer whose connection dropped (`Disconnected`) may take to get back to
    /// `Connected` before the session ends. `None` = 5 s.
    pub disconnect_grace: Option<Duration>,
}

const DEFAULT_DISCONNECT_GRACE: Duration = Duration::from_secs(5);

/// Ends a session once, whichever way it ended: stops the encoder, tells the agent (held keys
/// and buttons are released), wakes [`Session::wait_closed`] and closes the peer connection.
struct SessionEnd {
    id: SessionId,
    ended: AtomicBool,
    stop: Arc<AtomicBool>,
    control: mpsc::UnboundedSender<(SessionId, ControlMessage)>,
    closed: watch::Sender<bool>,
    pc: std::sync::Weak<RTCPeerConnection>,
}

impl SessionEnd {
    fn end(&self) {
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.control.send((self.id, ControlMessage::Closed));
        let _ = self.closed.send(true);
        if let Some(pc) = self.pc.upgrade() {
            tokio::spawn(async move {
                let _ = pc.close().await;
            });
        }
    }
}

/// How long to wait for each STUN server's answer in [`discover_public_ip`].
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

/// Asks the first answering `stun:` server in `urls` for this host's public IPv4 address.
/// `None` (logged) when there is no STUN server or none answers.
async fn discover_public_ip(urls: &[String]) -> Option<std::net::IpAddr> {
    let hosts: Vec<&str> = urls
        .iter()
        .filter_map(|u| u.strip_prefix("stun:"))
        .map(|h| h.split('?').next().unwrap_or(h))
        .collect();
    if hosts.is_empty() {
        return None;
    }
    for host in hosts {
        match tokio::time::timeout(STUN_TIMEOUT, stun_binding(host)).await {
            Ok(Ok(ip)) => {
                tracing::info!(stun = host, %ip, "discovered public IP");
                return Some(ip);
            }
            Ok(Err(e)) => tracing::warn!(stun = host, "STUN request failed: {e:#}"),
            Err(_) => tracing::warn!(stun = host, "STUN server did not answer"),
        }
    }
    tracing::warn!("no public IP discovered; pass --public-ip for viewers outside the LAN");
    None
}

/// Sends one STUN binding request to `host` (`name:port`, port 3478 when left out) and
/// returns the mapped address it reports.
async fn stun_binding(host: &str) -> Result<std::net::IpAddr> {
    use webrtc::stun::agent::TransactionId;
    use webrtc::stun::message::{Getter, Message, BINDING_REQUEST};
    use webrtc::stun::xoraddr::XorMappedAddress;

    let target = if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        host.to_owned()
    } else {
        format!("{host}:3478")
    };
    let addr = tokio::net::lookup_host(&target)
        .await
        .with_context(|| format!("resolve {target}"))?
        .find(|a| a.is_ipv4())
        .with_context(|| format!("{target} has no IPv4 address"))?;
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await?;
    let mut request = Message::new();
    request.build(&[Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)])?;
    socket.send_to(&request.raw, addr).await?;
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = socket.recv_from(&mut buf).await?;
        if from != addr {
            continue;
        }
        let mut response = Message::new();
        response.raw = buf[..n].to_vec();
        response.decode()?;
        if response.transaction_id != request.transaction_id {
            continue;
        }
        let mut mapped = XorMappedAddress::default();
        mapped.get_from(&response)?;
        return Ok(mapped.ip);
    }
}

/// Shared WebRTC API object; one per process.
pub struct WebRtcEngine {
    api: API,
    config: WebRtcConfig,
    next_id: AtomicU64,
    files: Option<FileAccessFactory>,
}

impl WebRtcEngine {
    pub async fn new(config: WebRtcConfig) -> Result<Self> {
        let mut media = MediaEngine::default();
        media.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;

        let mut settings = SettingEngine::default();
        // No mDNS: the agent never needs to resolve `.local` candidates (the browser reaches
        // its host candidates and shows up as peer-reflexive), and the mDNS socket logs a
        // warning for every LAN packet it cannot parse.
        settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        // A port another program holds must not take the whole agent down (the sign-in page
        // too): fall back to ephemeral ports, which still work on the LAN, and say so loudly.
        let mut udp_port = None;
        if let Some(port) = config.udp_port {
            match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
                Ok(socket) => {
                    tracing::info!("ICE UDP mux listening on 0.0.0.0:{port}");
                    settings.set_udp_network(UDPNetwork::Muxed(UDPMuxDefault::new(
                        UDPMuxParams::new(socket),
                    )));
                    settings.set_network_types(vec![NetworkType::Udp4]);
                    udp_port = Some(port);
                }
                Err(e) => tracing::error!(
                    "can't use UDP port {port} for WebRTC ({e}); another program may be using it. \
                     Using random UDP ports instead: viewers on this network can still connect, \
                     but a port forward to {port} won't work. Free the port or change \
                     DALKER_UDP_PORT, then restart Dalker."
                ),
            }
        }
        // With a single UDP port webrtc-ice gathers no server-reflexive candidates, so the
        // STUN servers would go unused: ask one for the public IP here and advertise it as
        // the host candidate instead, as if it had been given with --public-ip.
        let mut public_ips = config.public_ips.clone();
        if udp_port.is_some() && public_ips.is_empty() {
            if let Some(ip) = discover_public_ip(&config.ice_servers).await {
                public_ips.push(ip.to_string());
            }
        }
        if !public_ips.is_empty() {
            settings.set_nat_1to1_ips(public_ips.clone(), RTCIceCandidateType::Host);
            if udp_port.is_some() {
                // With a mux every local IP maps to the same public ip:port, and
                // webrtc-ice drops the duplicates by closing them — which closes
                // the shared muxed conn. Gather from a single local IPv4 only:
                // the first one offered (loopback is never offered).
                let chosen = std::sync::Mutex::new(None::<std::net::IpAddr>);
                tracing::info!(public = ?public_ips, "advertising NAT 1:1 host candidate");
                settings.set_ip_filter(Box::new(move |ip| {
                    if !ip.is_ipv4() {
                        return false;
                    }
                    let mut chosen = chosen.lock().unwrap();
                    *chosen.get_or_insert(ip) == ip
                }));
            }
        }

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        Ok(Self {
            api,
            config,
            next_id: AtomicU64::new(1),
            files: None,
        })
    }

    /// Offers file transfers to viewers (their `"files"` channel), run as the user `access`
    /// opens. Without it that channel is refused.
    pub fn with_files(mut self, access: FileAccessFactory) -> Self {
        self.files = Some(access);
        self
    }

    /// Creates a session for the browser's `offer_sdp` and returns the answer SDP.
    ///
    /// The session runs until the peer disconnects or [`Session::close`] is called.
    pub async fn accept_offer(
        &self,
        offer_sdp: String,
        frames: FrameWatch,
        clipboard: ClipboardWatch,
        control: mpsc::UnboundedSender<(SessionId, ControlMessage)>,
    ) -> Result<(String, Session)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rtc_config = RTCConfiguration {
            ice_servers: if self.config.ice_servers.is_empty() {
                vec![]
            } else {
                vec![RTCIceServer {
                    urls: self.config.ice_servers.clone(),
                    ..Default::default()
                }]
            },
            ..Default::default()
        };
        let pc = Arc::new(self.api.new_peer_connection(rtc_config).await?);

        // Video track. Added before applying the offer so it binds to the
        // browser's recvonly video transceiver.
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            "dalker".to_owned(),
        ));
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(true));

        // RTCP reader: keyframe requests (PLI/FIR) from the browser.
        {
            let keyframe = Arc::clone(&keyframe);
            tokio::spawn(async move {
                while let Ok((packets, _)) = sender.read_rtcp().await {
                    for p in packets {
                        let any = p.as_any();
                        if any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>() {
                            keyframe.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });
        }

        // This viewer's encoder settings: the host's, until the viewer asks for others.
        let base_settings = self.config.encoder;
        let (settings_tx, settings_rx) = watch::channel(base_settings);
        let settings_tx = Arc::new(settings_tx);

        // Control DataChannel → input worker; host clipboard → viewer; stream settings.
        // The "files" channel carries file transfers.
        let control_dc: ControlSlot = Arc::default();
        {
            let control = control.clone();
            let files = self.files.clone();
            let control_dc = Arc::clone(&control_dc);
            pc.on_data_channel(Box::new(move |dc| {
                let control = control.clone();
                let control_dc = Arc::clone(&control_dc);
                let clipboard = clipboard.clone();
                let settings_tx = Arc::clone(&settings_tx);
                let files = files.clone();
                Box::pin(async move {
                    if dc.label() == FILES_CHANNEL {
                        match files {
                            Some(access) => {
                                tracing::debug!(session = id, "file channel open");
                                files::attach(id, dc, access);
                            }
                            None => tracing::warn!(
                                session = id,
                                "file transfers are off; ignoring the files channel"
                            ),
                        }
                        return;
                    }
                    if dc.label() != CONTROL_CHANNEL {
                        tracing::warn!(
                            session = id,
                            label = dc.label(),
                            "ignoring unknown DataChannel"
                        );
                        return;
                    }
                    tracing::debug!(session = id, "control channel open");
                    *control_dc.lock().unwrap() = Some(Arc::clone(&dc));
                    let mut incoming = clipboard::Assembler::default();
                    let dc_weak = Arc::downgrade(&dc);
                    dc.on_message(Box::new(move |msg: DataChannelMessage| {
                        if let Some(req) = settings::parse(&msg.data) {
                            let applied = settings::apply(&base_settings, &req);
                            tracing::info!(
                                session = id,
                                fps = applied.max_fps,
                                kbps = applied.bitrate_bps / 1000,
                                downscale = applied.downscale,
                                "viewer stream settings"
                            );
                            settings_tx.send_replace(applied);
                            let reply = settings::applied_message(&base_settings, &applied);
                            let dc = dc_weak.clone();
                            return Box::pin(async move {
                                if let Some(dc) = dc.upgrade() {
                                    let _ = dc.send_text(reply).await;
                                }
                            });
                        }
                        if let Some(part) = clipboard::parse_part(&msg.data) {
                            match incoming.push(part) {
                                Ok(Some(text)) => {
                                    let _ = control.send((id, ControlMessage::Clipboard(text)));
                                }
                                Ok(None) => {}
                                Err(e) => tracing::info!(
                                    session = id,
                                    "viewer clipboard not applied: {e}"
                                ),
                            }
                            return Box::pin(async {});
                        }
                        match serde_json::from_slice::<InputEvent>(&msg.data) {
                            Ok(ev) => {
                                let _ = control.send((id, ControlMessage::Input(ev.sanitized())));
                            }
                            Err(e) => tracing::debug!(session = id, "bad control message: {e}"),
                        }
                        Box::pin(async {})
                    }));
                    let dc2 = Arc::clone(&dc);
                    dc.on_open(Box::new(move || {
                        Box::pin(async move {
                            tokio::spawn(clipboard::forward_to_viewer(id, dc2, clipboard));
                        })
                    }));
                })
            }));
        }

        // Connection state → lifecycle. Also when the host closed the session itself (`stop`
        // is set then), the agent hears of it, so held keys and buttons are released.
        let (closed_tx, closed_rx) = watch::channel(false);
        {
            let pc_weak = Arc::downgrade(&pc);
            let end = Arc::new(SessionEnd {
                id,
                ended: AtomicBool::new(false),
                stop: Arc::clone(&stop),
                control: control.clone(),
                closed: closed_tx,
                pc: pc_weak.clone(),
            });
            let grace = self
                .config
                .disconnect_grace
                .unwrap_or(DEFAULT_DISCONNECT_GRACE);
            pc.on_peer_connection_state_change(Box::new(move |state| {
                tracing::info!(session = id, ?state, "peer connection state");
                match state {
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => end.end(),
                    // Often only for a moment (Wi-Fi roaming, a few lost consent checks): give
                    // ICE a chance to get back to Connected before the session is torn down.
                    RTCPeerConnectionState::Disconnected => {
                        let end = Arc::clone(&end);
                        let pc = pc_weak.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(grace).await;
                            let back = pc.upgrade().is_some_and(|pc| {
                                pc.connection_state() == RTCPeerConnectionState::Connected
                            });
                            if back {
                                tracing::info!(session = id, "connection came back");
                            } else {
                                tracing::info!(
                                    session = id,
                                    ?grace,
                                    "connection did not come back"
                                );
                                end.end();
                            }
                        });
                    }
                    _ => {}
                }
                Box::pin(async {})
            }));
        }

        // SDP exchange (non-trickle). A peer connection that is dropped without `close()` leaves
        // webrtc-rs tasks running, so close it whenever this fails.
        let exchange = async {
            pc.set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
                .await?;
            let answer = pc.create_answer(None).await?;
            let mut gathered = pc.gathering_complete_promise().await;
            pc.set_local_description(answer).await?;
            let _ = tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await;
            pc.local_description().await.context("no local description")
        };
        let local = match exchange.await {
            Ok(local) => local,
            Err(e) => {
                stop.store(true, Ordering::SeqCst);
                let _ = pc.close().await;
                return Err(e);
            }
        };

        // A peer that never connects must not keep its slot.
        {
            let weak = Arc::downgrade(&pc);
            let stop = Arc::clone(&stop);
            let limit = self
                .config
                .connect_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                if let Some(pc) = weak.upgrade() {
                    if !stop.load(Ordering::SeqCst)
                        && pc.connection_state() != RTCPeerConnectionState::Connected
                    {
                        tracing::info!(session = id, ?limit, "viewer never connected; dropping it");
                        let _ = pc.close().await;
                    }
                }
            });
        }

        // Encoder thread (OpenH264 is blocking, keep it off the async runtime).
        let runtime = tokio::runtime::Handle::current();
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name(format!("dalker-enc-{id}"))
            .spawn(move || {
                encode_loop(
                    id,
                    settings_rx,
                    frames,
                    track,
                    keyframe,
                    thread_stop,
                    runtime,
                )
            })
            .context("spawn encoder thread");
        if let Err(e) = spawned {
            stop.store(true, Ordering::SeqCst);
            let _ = pc.close().await;
            return Err(e);
        }

        tracing::info!(session = id, "session negotiated");
        Ok((
            local.sdp,
            Session {
                id,
                pc,
                stop,
                closed: closed_rx,
                control_dc,
            },
        ))
    }
}

/// Handle to a live viewer session.
pub struct Session {
    pub id: SessionId,
    pc: Arc<RTCPeerConnection>,
    stop: Arc<AtomicBool>,
    closed: watch::Receiver<bool>,
    /// The viewer's control channel, once it is open (for the `session_end` message).
    control_dc: ControlSlot,
}

type ControlSlot = Arc<std::sync::Mutex<Option<Arc<RTCDataChannel>>>>;

impl Session {
    /// Resolves when the peer connection reaches a terminal state.
    pub async fn wait_closed(&mut self) {
        let _ = self.closed.wait_for(|c| *c).await;
    }

    pub async fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.pc.close().await;
    }

    /// Tells the viewer why the host ends the session, then closes it. The message gets up to
    /// [`END_FLUSH`] to leave; a viewer that is already gone is closed at once.
    pub async fn end(&self, reason: EndReason) {
        let dc = self.control_dc.lock().unwrap().clone();
        if let Some(dc) = dc.filter(|dc| dc.ready_state() == RTCDataChannelState::Open) {
            if dc.send_text(reason.message()).await.is_ok() {
                let deadline = Instant::now() + END_FLUSH;
                while dc.buffered_amount().await > 0 && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        self.close().await;
    }
}

/// Re-sends the last frame at least this often so a static desktop still gets
/// periodic frames (keeps jitter buffers and keyframe recovery healthy).
const IDLE_REFRESH: Duration = Duration::from_millis(500);

/// Longest wait for a new frame before the encoder loop looks at keyframe requests, new
/// settings and the stop flag again.
const FRAME_POLL: Duration = Duration::from_millis(20);

fn encode_loop(
    id: SessionId,
    mut settings: watch::Receiver<EncoderSettings>,
    mut frames: FrameWatch,
    track: Arc<TrackLocalStaticSample>,
    keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
) {
    let current = *settings.borrow_and_update();
    let mut enc = encoder::H264Encoder::new(current);
    let frame_interval = |s: &EncoderSettings| Duration::from_secs_f32(1.0 / s.max_fps.max(1.0));
    let mut interval = frame_interval(&current);
    let mut last_sent = Instant::now();
    let mut last_encode: Option<Instant> = None;
    // A fresh subscriber treats the current value as already seen; start from it.
    let mut last_frame: Option<Arc<RawFrame>> = frames.borrow_and_update().clone();
    let mut encoded = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let changed = frames.has_changed().unwrap_or(false);
        if changed {
            last_frame = frames.borrow_and_update().clone();
        } else if frames.has_changed().is_err() {
            break; // capture side shut down
        }
        // New viewer settings: re-encode right away so a still desktop shows them too.
        let resettled = settings.has_changed().unwrap_or(false);
        if resettled {
            let s = *settings.borrow_and_update();
            enc.set_settings(s);
            interval = frame_interval(&s);
        }
        let force_key = keyframe.swap(false, Ordering::Relaxed);
        if force_key {
            enc.request_keyframe();
        }

        let due = changed || force_key || resettled || last_sent.elapsed() >= IDLE_REFRESH;
        if let (true, Some(frame)) = (due, last_frame.as_ref()) {
            last_encode = Some(Instant::now());
            match enc.encode(frame) {
                Ok(Some((bytes, _key))) => {
                    let now = Instant::now();
                    let sample = Sample {
                        data: Bytes::from(bytes),
                        duration: now - last_sent,
                        ..Default::default()
                    };
                    last_sent = now;
                    if let Err(e) = runtime.block_on(track.write_sample(&sample)) {
                        tracing::debug!(session = id, "write_sample: {e}");
                    }
                    encoded += 1;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(session = id, "encode failed: {e:#}"),
            }
        }

        // Hold the frame rate to `max_fps`, then wake as soon as the next frame is captured.
        // (Sleeping a fixed interval instead let each frame wait up to one interval, 33 ms
        // at 30 fps, before it was even encoded.)
        if let Some(rest) = last_encode.and_then(|t| interval.checked_sub(t.elapsed())) {
            std::thread::sleep(rest);
        }
        if !frames.has_changed().unwrap_or(true) {
            // A clone so `changed()` doesn't mark the frame seen for the check at the top.
            let mut next = frames.clone();
            let _ =
                runtime.block_on(async { tokio::time::timeout(FRAME_POLL, next.changed()).await });
        }
    }
    tracing::info!(session = id, encoded, "encoder stopped");
}

#[cfg(test)]
mod tests {
    use super::EndReason;

    #[test]
    fn end_reason_messages() {
        for (reason, code) in [
            (EndReason::PasswordChanged, "password_changed"),
            (EndReason::WebOff, "web_off"),
            (EndReason::HostShutdown, "host_shutdown"),
        ] {
            let msg: serde_json::Value = serde_json::from_str(&reason.message()).unwrap();
            assert_eq!(
                msg,
                serde_json::json!({"type": "session_end", "reason": code})
            );
        }
    }
}
