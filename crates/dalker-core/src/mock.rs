//! Synthetic capture/input backends for testing WebRTC and the web UI without
//! touching any OS API.
//!
//! [`MockCapturer`] renders a moving gradient, a bouncing square and a running
//! `MM:SS.mmm` timer at a fixed frame rate. When paired with a [`MockInjector`]
//! (via [`MockCapturer::with_injector`]) it also draws the remote pointer and
//! highlights pressed buttons, so a full input round-trip is visible on screen.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::{
    CaptureError, DisplayInfo, InputError, InputEvent, InputInjector, NormPoint, PixelFormat,
    RawFrame, ScreenCapturer,
};

#[derive(Debug, Default)]
struct PointerState {
    pos: Option<NormPoint>,
    buttons_down: u32,
    last_key: Option<String>,
    events: u64,
}

/// Synthetic frame generator (default 30 FPS).
pub struct MockCapturer {
    width: u32,
    height: u32,
    frame_interval: Duration,
    start: Instant,
    next_deadline: Instant,
    frame_no: u64,
    pointer: Option<Arc<Mutex<PointerState>>>,
}

impl MockCapturer {
    /// Creates a generator. `width`/`height` are rounded down to even numbers
    /// (H.264 4:2:0 requires even dimensions).
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let width = (width.max(16)) & !1;
        let height = (height.max(16)) & !1;
        let now = Instant::now();
        Self {
            width,
            height,
            frame_interval: Duration::from_secs_f64(1.0 / fps.clamp(1, 240) as f64),
            start: now,
            next_deadline: now,
            frame_no: 0,
            pointer: None,
        }
    }

    /// Links this capturer to `injector` so injected pointer events are drawn.
    pub fn with_injector(mut self, injector: &MockInjector) -> Self {
        self.pointer = Some(injector.state.clone());
        self
    }

    /// Renders the frame for time `t` into a fresh BGRA buffer.
    pub fn render(&self, t: Duration, frame_no: u64) -> RawFrame {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let mut buf = vec![0u8; stride * h];
        let phase = (t.as_millis() / 8) as usize;

        // Diagonal scrolling gradient.
        for y in 0..h {
            let row = &mut buf[y * stride..(y + 1) * stride];
            for x in 0..w {
                let px = &mut row[x * 4..x * 4 + 4];
                px[0] = ((x + phase) & 0xFF) as u8; // B
                px[1] = ((y + phase / 2) & 0xFF) as u8; // G
                px[2] = (((x + y) / 4) & 0xFF) as u8; // R
                px[3] = 0xFF;
            }
        }

        // Bouncing square (period 4 s horizontally, 3 s vertically).
        let side = (h / 6).max(8);
        let bx = triangle(t.as_secs_f64(), 4.0) * (w - side) as f64;
        let by = triangle(t.as_secs_f64(), 3.0) * (h - side) as f64;
        fill_rect(&mut buf, stride, w, h, bx as usize, by as usize, side, side, [255, 255, 255]);

        // Timer text.
        let ms = t.as_millis();
        let text = format!(
            "{:02}:{:02}.{:03}  #{}",
            (ms / 60_000) % 100,
            (ms / 1000) % 60,
            ms % 1000,
            frame_no
        );
        let scale = (h / 90).max(2);
        let tw = text.len() * 6 * scale;
        fill_rect(&mut buf, stride, w, h, 8, 8, tw + 2 * scale, 9 * scale, [0, 0, 0]);
        draw_text(&mut buf, stride, w, h, 8 + scale, 8 + scale, scale, &text, [0, 255, 0]);

        // Remote pointer overlay.
        if let Some(state) = &self.pointer {
            let st = state.lock().unwrap();
            if let Some(pos) = st.pos {
                let (cx, cy) = pos.to_pixels(self.width, self.height);
                let color = if st.buttons_down != 0 { [0, 0, 255] } else { [255, 0, 255] };
                let r = (h / 60).max(4);
                fill_rect(&mut buf, stride, w, h, (cx as usize).saturating_sub(r), cy as usize, 2 * r + 1, 1, color);
                fill_rect(&mut buf, stride, w, h, cx as usize, (cy as usize).saturating_sub(r), 1, 2 * r + 1, color);
            }
            let info = format!("EV {}  {}", st.events, st.last_key.as_deref().unwrap_or("-"));
            let y0 = h.saturating_sub(10 * scale);
            fill_rect(&mut buf, stride, w, h, 8, y0, info.len() * 6 * scale + 2 * scale, 9 * scale, [0, 0, 0]);
            draw_text(&mut buf, stride, w, h, 8 + scale, y0 + scale, scale, &info.to_uppercase(), [255, 255, 0]);
        }

        RawFrame {
            width: self.width,
            height: self.height,
            stride,
            format: PixelFormat::Bgra8,
            data: Bytes::from(buf),
            timestamp: t,
        }
    }
}

impl ScreenCapturer for MockCapturer {
    fn name(&self) -> &str {
        "mock"
    }

    fn display(&self) -> DisplayInfo {
        DisplayInfo { width: self.width, height: self.height }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        let now = Instant::now();
        if self.next_deadline > now {
            let wait = self.next_deadline - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        // Don't try to "catch up" after a stall: schedule from now.
        let now = Instant::now();
        self.next_deadline = (self.next_deadline + self.frame_interval).max(now);
        self.frame_no += 1;
        Ok(Some(self.render(now - self.start, self.frame_no)))
    }
}

/// Input sink that records events instead of injecting them.
#[derive(Clone, Default)]
pub struct MockInjector {
    state: Arc<Mutex<PointerState>>,
    log: Arc<Mutex<Vec<InputEvent>>>,
}

impl MockInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// All events received so far.
    pub fn events(&self) -> Vec<InputEvent> {
        self.log.lock().unwrap().clone()
    }
}

impl InputInjector for MockInjector {
    fn name(&self) -> &str {
        "mock"
    }

    fn set_display(&mut self, _display: DisplayInfo) {}

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        {
            let mut st = self.state.lock().unwrap();
            st.events += 1;
            match event {
                InputEvent::MouseMove { x, y } => st.pos = Some(NormPoint::new(*x, *y)),
                InputEvent::MouseButton { button, down, x, y } => {
                    st.pos = Some(NormPoint::new(*x, *y));
                    let bit = 1 << (*button as u32);
                    if *down {
                        st.buttons_down |= bit;
                    } else {
                        st.buttons_down &= !bit;
                    }
                }
                InputEvent::Key { code, down } => {
                    if crate::keymap::lookup(code).is_none() {
                        return Err(InputError::UnknownKey(code.clone()));
                    }
                    if *down {
                        st.last_key = Some(code.clone());
                    }
                }
                InputEvent::ReleaseAll => st.buttons_down = 0,
                InputEvent::Wheel { .. } | InputEvent::Text { .. } | InputEvent::SecureAttention => {}
            }
        }
        self.log.lock().unwrap().push(event.clone());
        Ok(())
    }
}

/// Triangle wave in `0.0..=1.0` with the given period (seconds).
fn triangle(t: f64, period: f64) -> f64 {
    let p = (t / period).fract() * 2.0;
    if p < 1.0 {
        p
    } else {
        2.0 - p
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(buf: &mut [u8], stride: usize, w: usize, h: usize, x0: usize, y0: usize, rw: usize, rh: usize, rgb: [u8; 3]) {
    for y in y0..(y0 + rh).min(h) {
        for x in x0..(x0 + rw).min(w) {
            let i = y * stride + x * 4;
            buf[i] = rgb[2];
            buf[i + 1] = rgb[1];
            buf[i + 2] = rgb[0];
            buf[i + 3] = 0xFF;
        }
    }
}

/// 5x7 glyphs, one byte per row, low 5 bits used (bit 4 = leftmost column).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '#' => [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        c if c.is_ascii_uppercase() => letter(c),
        _ => [0; 7],
    }
}

fn letter(c: char) -> [u8; 7] {
    match c {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        _ => [0; 7],
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(buf: &mut [u8], stride: usize, w: usize, h: usize, x0: usize, y0: usize, scale: usize, text: &str, rgb: [u8; 3]) {
    for (i, c) in text.chars().enumerate() {
        let g = glyph(c);
        let gx = x0 + i * 6 * scale;
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5 {
                if bits & (0x10 >> col) != 0 {
                    fill_rect(buf, stride, w, h, gx + col * scale, y0 + row * scale, scale, scale, rgb);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_valid_geometry() {
        let mut cap = MockCapturer::new(321, 241, 30);
        assert_eq!(cap.display(), DisplayInfo { width: 320, height: 240 });
        let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        assert!(f.is_consistent());
        assert_eq!(f.format, PixelFormat::Bgra8);
        assert_eq!(f.data.len(), 320 * 240 * 4);
    }

    #[test]
    fn paces_to_frame_rate() {
        let mut cap = MockCapturer::new(64, 64, 50);
        let start = Instant::now();
        for _ in 0..6 {
            cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        }
        // First frame is immediate, the next 5 are 20 ms apart.
        assert!(start.elapsed() >= Duration::from_millis(95), "{:?}", start.elapsed());
    }

    #[test]
    fn short_timeout_returns_none() {
        let mut cap = MockCapturer::new(64, 64, 1);
        assert!(cap.next_frame(Duration::from_millis(1)).unwrap().is_some());
        assert!(cap.next_frame(Duration::from_millis(1)).unwrap().is_none());
    }

    #[test]
    fn frames_change_over_time() {
        let cap = MockCapturer::new(64, 64, 30);
        let a = cap.render(Duration::from_millis(0), 1);
        let b = cap.render(Duration::from_millis(500), 2);
        assert_ne!(a.data, b.data);
    }

    #[test]
    fn injector_feeds_overlay() {
        let mut inj = MockInjector::new();
        let cap = MockCapturer::new(200, 200, 30).with_injector(&inj);
        let before = cap.render(Duration::ZERO, 1);
        inj.inject(&InputEvent::MouseMove { x: 0.5, y: 0.5 }).unwrap();
        let after = cap.render(Duration::ZERO, 1);
        assert_ne!(before.data, after.data);
        assert!(inj.inject(&InputEvent::Key { code: "Bogus".into(), down: true }).is_err());
        assert_eq!(inj.events().len(), 1);
    }
}
