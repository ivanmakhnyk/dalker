//! One AI agent connected to the desktop: its viewer slot, held input and coordinate frame.
//!
//! The 2026-07-28 MCP protocol has no sessions, and older clients open a new one on every
//! reconnect, so the desktop session belongs to the *token*: every request with the same token
//! shares it. It starts with the first tool call, counts as a viewer (the border goes on), and
//! ends when it is idle too long, the agent goes away (its connection closes, or it has no
//! request open for a while), its token is revoked or expires, or the person at the host
//! disconnects AI agents. Ending it releases every key and button it held.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dalker_core::{InputEvent, NormPoint, RawFrame};

use crate::screen::{self, Format, Frame, Region, Shot};
use crate::{DesktopHandle, FrameWatch, HostCommand};

/// How long the first screenshot of a session waits for the capture to deliver a fresh frame.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(3);
/// Longest wait for the host to apply a tool call's input.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long any screenshot waits when no frame exists yet (capture starting up).
const NO_FRAME_WAIT: Duration = Duration::from_secs(5);

/// Calls per second a session may make, by kind. A token bucket each: `rate` per second
/// sustained, `burst` at once.
#[derive(Debug, Clone, Copy)]
pub struct RateLimits {
    pub input_per_sec: f64,
    pub input_burst: f64,
    pub screenshots_per_sec: f64,
    pub screenshot_burst: f64,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            input_per_sec: 20.0,
            input_burst: 40.0,
            screenshots_per_sec: 4.0,
            screenshot_burst: 8.0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    last: Instant,
}

impl Bucket {
    pub(crate) fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            last: Instant::now(),
        }
    }

    pub(crate) fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub(crate) struct Session {
    /// Input-thread session id (like a WebRTC viewer's).
    pub id: u64,
    pub token_id: String,
    pub token_name: String,
    /// Shared with [`State`], so a tool call still running after `close()` reads no more frames.
    closed: Arc<AtomicBool>,
    last_used: Mutex<Instant>,
    desktop: Arc<dyn DesktopHandle>,
    /// Sessions can be ended from threads outside the runtime (the tray).
    runtime: tokio::runtime::Handle,
    /// Serialises this agent's tool calls, so two calls can't interleave their events.
    pub state: tokio::sync::Mutex<State>,
}

pub(crate) struct State {
    /// Held for the session's lifetime: keeps the (border-gated) capture running.
    frames: FrameWatch,
    /// The session's `closed` flag.
    closed: Arc<AtomicBool>,
    /// Whether a frame published after this session started has been seen.
    fresh: bool,
    /// Size of the last full screenshot: what the model's coordinates refer to.
    pub frame: Option<Frame>,
    /// `max_width` of the last full screenshot, reused by `screenshot_after`.
    pub max_width: u32,
    input: Bucket,
    shots: Bucket,
}

impl Session {
    pub(crate) fn new(
        id: u64,
        token_id: String,
        token_name: String,
        desktop: Arc<dyn DesktopHandle>,
        limits: RateLimits,
    ) -> Self {
        let closed = Arc::new(AtomicBool::new(false));
        let state = State {
            frames: desktop.frames(),
            closed: Arc::clone(&closed),
            fresh: false,
            frame: None,
            max_width: screen::DEFAULT_MAX_WIDTH,
            input: Bucket::new(limits.input_per_sec, limits.input_burst),
            shots: Bucket::new(limits.screenshots_per_sec, limits.screenshot_burst),
        };
        Self {
            id,
            token_id,
            token_name,
            closed,
            last_used: Mutex::new(Instant::now()),
            desktop,
            runtime: tokio::runtime::Handle::current(),
            state: tokio::sync::Mutex::new(state),
        }
    }

    pub(crate) fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    pub(crate) fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(*self.last_used.lock().unwrap())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Marks the session closed and releases its input at once. Returns false if it already was.
    pub(crate) fn close(self: &Arc<Self>) -> bool {
        if self.closed.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.desktop.send(self.id, HostCommand::EndSession);
        self.desktop.close_viewer(self.id);
        // A tool call still running may send a few more events: release again once it is done.
        let me = Arc::clone(self);
        self.runtime.spawn(async move {
            let _state = me.state.lock().await;
            me.desktop.send(me.id, HostCommand::EndSession);
        });
        true
    }

    /// Sends one input event, unless the session has ended.
    pub(crate) fn input(&self, ev: InputEvent) -> Result<(), String> {
        if self.is_closed() {
            return Err(ENDED.into());
        }
        self.desktop
            .send(self.id, HostCommand::Input(ev.sanitized()));
        Ok(())
    }

    pub(crate) fn set_clipboard(&self, text: String) -> Result<(), String> {
        if self.is_closed() {
            return Err(ENDED.into());
        }
        self.desktop.send(self.id, HostCommand::SetClipboard(text));
        Ok(())
    }

    /// Waits until the host has applied this session's input (typing on Windows runs at
    /// ~30 characters a second, so a long text takes a while).
    pub(crate) async fn flush(&self) -> Result<(), String> {
        tokio::time::timeout(FLUSH_TIMEOUT, self.desktop.flush(self.id))
            .await
            .map_err(|_| "the host is still applying earlier input; try again shortly".to_string())
    }

    pub(crate) fn desktop(&self) -> &Arc<dyn DesktopHandle> {
        &self.desktop
    }
}

pub(crate) const ENDED: &str = "this AI agent session was ended on the host";

impl State {
    pub(crate) fn take_input(&mut self) -> Result<(), String> {
        if self.input.take(Instant::now()) {
            Ok(())
        } else {
            Err("rate limited: too many input actions, slow down".into())
        }
    }

    pub(crate) fn take_screenshot(&mut self) -> Result<(), String> {
        if self.shots.take(Instant::now()) {
            Ok(())
        } else {
            Err("rate limited: too many screenshots, slow down".into())
        }
    }

    /// The latest frame from the capture thread, waiting for one where needed.
    pub(crate) async fn latest_frame(
        &mut self,
        desktop: &dyn DesktopHandle,
    ) -> Result<Arc<RawFrame>, String> {
        let frame = self.wait_frame(desktop).await?;
        // The session may have ended (border off, viewer slot given back) while we waited.
        if self.closed.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        Ok(frame)
    }

    async fn wait_frame(&mut self, desktop: &dyn DesktopHandle) -> Result<Arc<RawFrame>, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if !desktop.screen_ready() {
            return Err("the screen is not available right now (the on-screen border that tells people at the \
                        host they are being watched can't be shown, or there is no display yet); try again later"
                .into());
        }
        if !self.fresh {
            // What is on the channel may be from long ago (capture pauses with nobody watching).
            self.frames.borrow_and_update();
            let _ = tokio::time::timeout(FIRST_FRAME_WAIT, self.frames.changed()).await;
            self.fresh = true;
        }
        if let Some(f) = self.frames.borrow_and_update().clone() {
            return Ok(f);
        }
        let wait = async {
            loop {
                if self.frames.changed().await.is_err() {
                    return None;
                }
                if let Some(f) = self.frames.borrow_and_update().clone() {
                    return Some(f);
                }
            }
        };
        match tokio::time::timeout(NO_FRAME_WAIT, wait).await {
            Ok(Some(f)) => Ok(f),
            _ => Err(
                "no screen image yet (capture is starting or failing); try again in a moment"
                    .into(),
            ),
        }
    }

    /// The coordinate frame: the last full screenshot, or what a default one would be.
    pub(crate) async fn coordinate_frame(
        &mut self,
        desktop: &dyn DesktopHandle,
    ) -> Result<Frame, String> {
        if let Some(f) = self.frame {
            return Ok(f);
        }
        let raw = self.latest_frame(desktop).await?;
        Ok(Frame::fit(raw.width, raw.height, screen::DEFAULT_MAX_WIDTH))
    }

    /// Model pixel coordinates → normalized point.
    pub(crate) async fn point(
        &mut self,
        desktop: &dyn DesktopHandle,
        x: f64,
        y: f64,
    ) -> Result<NormPoint, String> {
        self.coordinate_frame(desktop).await?.to_norm(x, y)
    }

    /// Takes a screenshot. A full one becomes the new coordinate frame; a region does not.
    pub(crate) async fn screenshot(
        &mut self,
        desktop: &dyn DesktopHandle,
        max_width: u32,
        format: Option<Format>,
        region: Option<Region>,
    ) -> Result<(Shot, String), String> {
        self.take_screenshot()?;
        let max_width = max_width.clamp(screen::MIN_MAX_WIDTH, screen::MAX_MAX_WIDTH);
        let raw = self.latest_frame(desktop).await?;
        let (nw, nh) = (raw.width, raw.height);
        let native = match region {
            Some(r) => {
                let frame = match self.frame {
                    Some(f) => f,
                    None => Frame::fit(nw, nh, screen::DEFAULT_MAX_WIDTH),
                };
                Some((frame, r, screen::region_to_native(frame, r, nw, nh)?))
            }
            None => None,
        };
        let crop = native.map_or(
            Region {
                x: 0,
                y: 0,
                w: nw,
                h: nh,
            },
            |(_, _, n)| n,
        );
        let format = format.unwrap_or(if region.is_some() {
            Format::Png
        } else {
            Format::Jpeg
        });
        let shot =
            tokio::task::spawn_blocking(move || screen::encode(&raw, crop, max_width, format))
                .await
                .map_err(|e| format!("screenshot failed: {e}"))??;
        let text = match native {
            None => {
                self.frame = Some(Frame {
                    width: shot.width,
                    height: shot.height,
                });
                self.max_width = max_width;
                let scale = nw as f64 / shot.width as f64;
                format!(
                    "Screenshot {}x{} of a {nw}x{nh} screen (1 image px = {scale:.3} screen px). Mouse tools take pixel \
                     coordinates of this image, origin top-left.",
                    shot.width, shot.height
                )
            }
            Some((frame, r, _)) => {
                let zoom = shot.width as f64 / r.w.min(frame.width - r.x) as f64;
                format!(
                    "Region x={} y={} w={} h={} of the screenshot, shown at {}x{} (zoom {zoom:.2}x). Mouse tools still \
                     take coordinates of the full {}x{} screenshot: add the region's x/y and divide by the zoom.",
                    r.x, r.y, r.w, r.h, shot.width, shot.height, frame.width, frame.height
                )
            }
        };
        Ok((shot, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_burst_then_rate() {
        let mut b = Bucket::new(2.0, 3.0);
        let t0 = b.last;
        assert!(b.take(t0) && b.take(t0) && b.take(t0), "burst of 3");
        assert!(!b.take(t0), "then empty");
        assert!(!b.take(t0 + Duration::from_millis(400)), "0.8 tokens");
        assert!(
            b.take(t0 + Duration::from_millis(500)),
            "1 token after 0.5 s at 2/s"
        );
        assert!(b.take(t0 + Duration::from_secs(10)) && b.take(t0 + Duration::from_secs(10)));
        assert!(b.take(t0 + Duration::from_secs(10)));
        assert!(
            !b.take(t0 + Duration::from_secs(10)),
            "refill is capped at the burst"
        );
    }
}
