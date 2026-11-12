//! End-to-end: a webrtc-rs client plays the browser's role against the engine
//! fed by the mock capturer — negotiates, receives H.264 RTP, sends input over
//! the "control" DataChannel, syncs the clipboard both ways, and observes session teardown.

use std::sync::Arc;
use std::time::Duration;

use dalker_core::{InputEvent, MockCapturer, MouseButton, RawFrame, ScreenCapturer};
use dalker_webrtc::{ClipboardState, ControlMessage, EndReason, WebRtcConfig, WebRtcEngine};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mock_stream_roundtrip() {
    roundtrip(WebRtcConfig::default(), false).await;
}

/// The VM/port-forward setup: one muxed UDP port advertised as 127.0.0.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mock_stream_roundtrip_udp_mux_nat1to1() {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = WebRtcConfig {
        udp_port: Some(port),
        public_ips: vec!["127.0.0.1".into()],
        ..Default::default()
    };
    roundtrip(config, false).await;
}

/// The fixed UDP port is taken by another program: the engine still starts, on random ports,
/// and a viewer on this machine still connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_udp_port_falls_back_to_random_ports() {
    let holder = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let config = WebRtcConfig {
        udp_port: Some(port),
        ..Default::default()
    };
    roundtrip(config, false).await;
    drop(holder);
}

/// The host ends the session (password changed): the viewer is told why before it closes,
/// and the agent still hears that the session closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_ends_session_with_reason() {
    roundtrip(WebRtcConfig::default(), true).await;
}

async fn roundtrip(config: WebRtcConfig, host_ends: bool) {
    // Frame source: mock capturer on its own thread, like the agent.
    let (frame_tx, _) = watch::channel::<Option<Arc<RawFrame>>>(None);
    let frames = frame_tx.subscribe();
    let tx = frame_tx.clone();
    std::thread::spawn(move || {
        let mut cap = MockCapturer::new(640, 360, 30);
        while tx.receiver_count() > 0 {
            if let Ok(Some(f)) = cap.next_frame(Duration::from_millis(100)) {
                tx.send_replace(Some(Arc::new(f)));
            }
        }
    });

    let engine = WebRtcEngine::new(config).await.unwrap();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (clip_tx, clip_rx) = watch::channel(ClipboardState {
        available: true,
        backend: "mock".into(),
        ..Default::default()
    });

    // "Browser" peer.
    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    let client = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    client
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
    let dc = client.create_data_channel("control", None).await.unwrap();
    let (open_tx, mut open_rx) = mpsc::channel::<()>(1);
    dc.on_open(Box::new(move || {
        let _ = open_tx.try_send(());
        Box::pin(async {})
    }));
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    dc.on_message(Box::new(move |m| {
        if let Ok(v) = serde_json::from_slice(&m.data) {
            let _ = msg_tx.send(v);
        }
        Box::pin(async {})
    }));

    let (rtp_tx, mut rtp_rx) = mpsc::unbounded_channel::<(String, usize)>();
    client.on_track(Box::new(move |track, _, _| {
        let rtp_tx = rtp_tx.clone();
        let mime = track.codec().capability.mime_type.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                if rtp_tx.send((mime.clone(), pkt.payload.len())).is_err() {
                    break;
                }
            }
        });
        Box::pin(async {})
    }));

    let offer = client.create_offer(None).await.unwrap();
    let mut gathered = client.gathering_complete_promise().await;
    client.set_local_description(offer).await.unwrap();
    let _ = gathered.recv().await;
    let offer_sdp = client.local_description().await.unwrap().sdp;

    let (answer_sdp, mut session) = engine
        .accept_offer(offer_sdp, frames, clip_rx, control_tx)
        .await
        .unwrap();
    assert!(
        answer_sdp.contains("H264"),
        "answer must negotiate H.264:\n{answer_sdp}"
    );
    client
        .set_remote_description(RTCSessionDescription::answer(answer_sdp).unwrap())
        .await
        .unwrap();

    // Media flows.
    let mut packets = 0;
    let mut bytes = 0;
    while packets < 50 {
        let (mime, len) = timeout(Duration::from_secs(15), rtp_rx.recv())
            .await
            .expect("timed out waiting for RTP")
            .unwrap();
        assert_eq!(mime.to_ascii_lowercase(), "video/h264");
        packets += 1;
        bytes += len;
    }
    assert!(
        bytes > 10_000,
        "suspiciously little video data: {bytes} bytes"
    );

    // Input flows.
    timeout(Duration::from_secs(10), open_rx.recv())
        .await
        .expect("control channel open");
    dc.send_text(
        r#"{"type":"mouse_button","button":"left","down":true,"x":0.25,"y":2.0}"#.to_string(),
    )
    .await
    .unwrap();
    dc.send_text("not json".to_string()).await.unwrap();
    dc.send_text(r#"{"type":"key","code":"KeyA","down":true}"#.to_string())
        .await
        .unwrap();

    let (_, first) = timeout(Duration::from_secs(5), control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match first {
        // y=2.0 must arrive clamped to 1.0; the garbage message is dropped.
        ControlMessage::Input(ev) => assert_eq!(
            ev,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
                x: 0.25,
                y: 1.0
            }
        ),
        other => panic!("unexpected {other:?}"),
    }
    let (_, second) = timeout(Duration::from_secs(5), control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(second, ControlMessage::Input(InputEvent::Key { ref code, down: true }) if code == "KeyA")
    );

    // Clipboard: the status comes first, then host changes; viewer text reaches the agent
    // in order with the input.
    let status = timeout(Duration::from_secs(5), msg_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status["type"], "clipboard_status");
    assert_eq!(status["available"], true);
    dc.send_text(r#"{"type":"clipboard","text":"from ","more":true}"#.to_string())
        .await
        .unwrap();
    dc.send_text(r#"{"type":"clipboard","text":"viewer"}"#.to_string())
        .await
        .unwrap();
    dc.send_text(r#"{"type":"key","code":"KeyV","down":true}"#.to_string())
        .await
        .unwrap();
    let (_, clip) = timeout(Duration::from_secs(5), control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(clip, ControlMessage::Clipboard(ref t) if t == "from viewer"),
        "{clip:?}"
    );
    let (_, key) = timeout(Duration::from_secs(5), control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(key, ControlMessage::Input(InputEvent::Key { ref code, .. }) if code == "KeyV")
    );
    clip_tx.send_modify(|s| {
        s.serial += 1;
        s.text = Some("from host".into());
    });
    let text = timeout(Duration::from_secs(5), msg_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        text,
        serde_json::json!({"type": "clipboard", "text": "from host", "more": false})
    );

    // Stream settings: clamped to the host's limits and confirmed; nothing reaches the agent.
    dc.send_text(r#"{"type":"settings","fps":144,"bitrate_kbps":1000,"downscale":2}"#.to_string())
        .await
        .unwrap();
    let applied = timeout(Duration::from_secs(5), msg_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(applied["type"], "settings_applied");
    assert_eq!(applied["fps"], 30, "the default host maximum");
    assert_eq!(applied["bitrate_kbps"], 1000);
    assert_eq!(applied["downscale"], 2);

    // Teardown reaches the agent so it can release held keys.
    if host_ends {
        session.end(EndReason::PasswordChanged).await;
        let bye = timeout(Duration::from_secs(5), msg_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bye,
            serde_json::json!({"type": "session_end", "reason": "password_changed"})
        );
    } else {
        client.close().await.unwrap();
    }
    timeout(Duration::from_secs(30), session.wait_closed())
        .await
        .expect("session close detected");
    let closed = timeout(Duration::from_secs(5), async {
        loop {
            match control_rx.recv().await {
                Some((_, ControlMessage::Closed)) => break true,
                Some(_) => continue,
                None => break false,
            }
        }
    })
    .await
    .unwrap();
    assert!(closed);
    drop(frame_tx);
}

/// A viewer that gets the answer but never applies it (like a browser that cannot complete
/// ICE) must be dropped after `connect_timeout`, releasing its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn viewer_that_never_connects_is_dropped() {
    let (frame_tx, _) = watch::channel::<Option<Arc<RawFrame>>>(None);
    let engine = WebRtcEngine::new(WebRtcConfig {
        connect_timeout: Some(Duration::from_secs(2)),
        ..Default::default()
    })
    .await
    .unwrap();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();

    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    let client = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    client
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
    let _dc = client.create_data_channel("control", None).await.unwrap();
    let offer = client.create_offer(None).await.unwrap();
    let mut gathered = client.gathering_complete_promise().await;
    client.set_local_description(offer).await.unwrap();
    let _ = gathered.recv().await;
    let offer_sdp = client.local_description().await.unwrap().sdp;

    let started = std::time::Instant::now();
    let (_answer, mut session) = engine
        .accept_offer(
            offer_sdp,
            frame_tx.subscribe(),
            watch::channel(ClipboardState::default()).1,
            control_tx,
        )
        .await
        .unwrap();
    // The answer is deliberately never applied: the browser "cannot connect".

    timeout(Duration::from_secs(15), session.wait_closed())
        .await
        .expect("session was never dropped");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "dropped too early: {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "dropped too late: {:?}",
        started.elapsed()
    );

    let closed = timeout(Duration::from_secs(5), async {
        loop {
            match control_rx.recv().await {
                Some((_, ControlMessage::Closed)) => break true,
                Some(_) => continue,
                None => break false,
            }
        }
    })
    .await
    .unwrap();
    assert!(
        closed,
        "agent must be told the viewer is gone (releases keys, hides the border)"
    );
}

/// A captured frame goes out as soon as it is encoded, not at the encoder's next fixed tick
/// (which made every frame wait up to a whole frame interval: 200 ms at 5 fps here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_frame_is_sent_without_waiting_for_a_tick() {
    let (frame_tx, _) = watch::channel::<Option<Arc<RawFrame>>>(None);
    let mut cap = MockCapturer::new(320, 180, 30);
    let mut publish = || {
        frame_tx.send_replace(Some(Arc::new(
            cap.next_frame(Duration::from_secs(1)).unwrap().unwrap(),
        )))
    };
    publish();
    let mut config = WebRtcConfig::default();
    config.encoder.max_fps = 5.0;
    let engine = WebRtcEngine::new(config).await.unwrap();
    let (control_tx, _control_rx) = mpsc::unbounded_channel();

    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    let client = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    client
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
    let _dc = client.create_data_channel("control", None).await.unwrap();
    let (rtp_tx, mut rtp_rx) = mpsc::unbounded_channel::<u32>();
    client.on_track(Box::new(move |track, _, _| {
        let rtp_tx = rtp_tx.clone();
        tokio::spawn(async move {
            while let Ok((pkt, _)) = track.read_rtp().await {
                if rtp_tx.send(pkt.header.timestamp).is_err() {
                    break;
                }
            }
        });
        Box::pin(async {})
    }));
    let offer = client.create_offer(None).await.unwrap();
    let mut gathered = client.gathering_complete_promise().await;
    client.set_local_description(offer).await.unwrap();
    let _ = gathered.recv().await;
    let offer_sdp = client.local_description().await.unwrap().sdp;
    let (answer, _session) = engine
        .accept_offer(
            offer_sdp,
            frame_tx.subscribe(),
            watch::channel(ClipboardState::default()).1,
            control_tx,
        )
        .await
        .unwrap();
    client
        .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
        .await
        .unwrap();
    let mut last_ts = timeout(Duration::from_secs(15), rtp_rx.recv())
        .await
        .expect("first frame")
        .unwrap();

    let mut delays = Vec::new();
    for _ in 0..8 {
        // Past the 5 fps interval, before the 500 ms idle refresh.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(ts) = rtp_rx.try_recv() {
            last_ts = ts;
        }
        let sent = std::time::Instant::now();
        publish();
        loop {
            let ts = timeout(Duration::from_secs(5), rtp_rx.recv())
                .await
                .expect("frame sent")
                .unwrap();
            if ts != last_ts {
                last_ts = ts;
                break;
            }
        }
        delays.push(sent.elapsed());
    }
    delays.sort();
    let median = delays[delays.len() / 2];
    println!("new frame -> first RTP packet: {delays:?}");
    assert!(
        median < Duration::from_millis(60),
        "frames wait for the encoder: {delays:?}"
    );
    drop(client);
}
