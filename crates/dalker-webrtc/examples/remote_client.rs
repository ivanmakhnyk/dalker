//! Headless viewer for testing a running `dalkerd` (e.g. inside a VM):
//! logs in over HTTP, negotiates WebRTC, records the H.264 stream to a file
//! and optionally sends a short input script.
//!
//! ```text
//! DALKER_PASSWORD_FILE=pw.txt cargo run -p dalker-webrtc --example remote_client -- \
//!     127.0.0.1:8742 out.h264 [--input]
//! ```
//! The password is read from the file named by `DALKER_PASSWORD_FILE` (never printed).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::media::io::h264_writer::H264Writer;
use webrtc::media::io::Writer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

/// Minimal HTTP/1.1 POST with JSON; returns (status, headers, body).
async fn post(
    addr: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
) -> Result<(u16, String, String)> {
    let mut s = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{cookie}Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").context("bad HTTP response")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_owned(), body.to_owned()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        bail!("usage: remote_client <host:port> <out.h264> [--input]");
    }
    let (addr, out) = (&args[1], &args[2]);
    let send_input = args.iter().any(|a| a == "--input");
    let pw_file = std::env::var("DALKER_PASSWORD_FILE").context("DALKER_PASSWORD_FILE not set")?;
    let password = std::fs::read_to_string(&pw_file)?.trim().to_owned();

    // Login.
    let login = serde_json::json!({"password": password}).to_string();
    let (status, head, body) = post(addr, "/api/login", &login, None).await?;
    if status != 200 {
        bail!("login failed: HTTP {status} {body}");
    }
    let cookie = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .and_then(|c| c.split(';').next())
        .context("no session cookie")?
        .to_owned();
    println!("login ok");

    // Peer.
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;
    let dc = pc.create_data_channel("control", None).await?;

    let packets = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let first_rtp = Arc::new(Mutex::new(None::<Instant>));
    let writer = Arc::new(Mutex::new(H264Writer::new(std::fs::File::create(out)?)));
    {
        let (packets, frames, bytes, first_rtp, writer) = (
            packets.clone(),
            frames.clone(),
            bytes.clone(),
            first_rtp.clone(),
            writer.clone(),
        );
        pc.on_track(Box::new(move |track, _, _| {
            let (packets, frames, bytes, first_rtp, writer) = (
                packets.clone(),
                frames.clone(),
                bytes.clone(),
                first_rtp.clone(),
                writer.clone(),
            );
            tokio::spawn(async move {
                while let Ok((pkt, _)) = track.read_rtp().await {
                    first_rtp.lock().unwrap().get_or_insert_with(Instant::now);
                    packets.fetch_add(1, Ordering::Relaxed);
                    bytes.fetch_add(pkt.payload.len() as u64, Ordering::Relaxed);
                    if pkt.header.marker {
                        frames.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = writer.lock().unwrap().write_rtp(&pkt);
                }
            });
            Box::pin(async {})
        }));
    }

    let started = Instant::now();
    let offer = pc.create_offer(None).await?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    let _ = gathered.recv().await;
    let sdp = pc.local_description().await.context("no local sdp")?.sdp;
    let (status, _, body) = post(
        addr,
        "/api/offer",
        &serde_json::json!({ "sdp": sdp }).to_string(),
        Some(&cookie),
    )
    .await?;
    if status != 200 {
        bail!("offer failed: HTTP {status} {body}");
    }
    let answer: serde_json::Value = serde_json::from_str(&body)?;
    let answer_sdp = answer["sdp"]
        .as_str()
        .context("no sdp in answer")?
        .to_owned();
    for line in answer_sdp.lines().filter(|l| l.starts_with("a=candidate")) {
        println!("remote {line}");
    }
    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;

    // Watch the stream for a while.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if frames.load(Ordering::Relaxed) > 0 {
            break;
        }
    }
    if let Some(t) = *first_rtp.lock().unwrap() {
        println!("first video after {:?}", t - started);
    } else {
        bail!(
            "no video received within 10s (state: {:?})",
            pc.connection_state()
        );
    }

    if send_input {
        let send = |v: serde_json::Value| {
            let dc = dc.clone();
            async move { dc.send_text(v.to_string()).await }
        };
        // Sweep the pointer around a circle, then tap the Windows/Super key twice
        // (opens and closes the Start menu / Activities overview).
        for i in 0..=60 {
            let a = i as f32 / 60.0 * std::f32::consts::TAU;
            send(serde_json::json!({"type":"mouse_move","x":0.5 + 0.3 * a.cos(),"y":0.5 + 0.3 * a.sin()})).await?;
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        send(serde_json::json!({"type":"mouse_move","x":0.5,"y":0.5})).await?;
        send(serde_json::json!({"type":"key","code":"MetaLeft","down":true})).await?;
        send(serde_json::json!({"type":"key","code":"MetaLeft","down":false})).await?;
        println!("input: pointer circle + Win key sent");
        tokio::time::sleep(Duration::from_millis(2500)).await;
    } else {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let secs = started.elapsed().as_secs_f64();
    println!(
        "received {} RTP packets, {} frames, {:.2} MB in {:.1}s (state {:?})",
        packets.load(Ordering::Relaxed),
        frames.load(Ordering::Relaxed),
        bytes.load(Ordering::Relaxed) as f64 / 1e6,
        secs,
        pc.connection_state()
    );
    let _ = writer.lock().unwrap().close();
    pc.close().await?;
    Ok(())
}
