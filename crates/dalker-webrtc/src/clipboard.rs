//! Clipboard text on the `"control"` DataChannel.
//!
//! Wire format (JSON). A text longer than [`CHUNK_CHARS`] is split into several messages;
//! every part but the last has `"more": true`:
//! ```json
//! {"type":"clipboard","text":"hello","more":false}          both ways
//! {"type":"clipboard_status","available":true,"backend":"windows"}   host → viewer
//! {"type":"clipboard_status","available":false,"reason":"..."}       host → viewer
//! ```
//! The viewer gets the status when the channel opens and whenever it changes, and the text
//! each time the host's clipboard changes while it is connected (not what was there before).

use std::sync::Arc;

use dalker_core::MAX_CLIPBOARD_BYTES;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use webrtc::data_channel::RTCDataChannel;

use crate::SessionId;

/// Characters per message. A JSON-escaped char takes at most 6 bytes, so a part stays well
/// under the 64 KiB SCTP message limit webrtc-rs and browsers agree on.
pub const CHUNK_CHARS: usize = 8 * 1024;

/// The host's clipboard as the agent's clipboard worker publishes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClipboardState {
    /// False when the host's clipboard cannot be used; `reason` says why.
    pub available: bool,
    pub backend: String,
    pub reason: Option<String>,
    /// Bumped on every change of `text` seen while viewers are connected.
    pub serial: u64,
    pub text: Option<Arc<str>>,
}

pub type ClipboardWatch = watch::Receiver<ClipboardState>;

#[derive(Debug, Deserialize)]
pub(crate) struct Part {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    more: bool,
}

/// A clipboard message from the viewer, or `None` for anything else (input events).
pub(crate) fn parse_part(data: &[u8]) -> Option<Part> {
    // Cheap test first: input events are far more frequent.
    if !data.windows(11).any(|w| w == b"\"clipboard\"") {
        return None;
    }
    serde_json::from_slice::<Part>(data)
        .ok()
        .filter(|p| p.kind == "clipboard")
}

/// Joins the parts of one clipboard text.
#[derive(Default)]
pub(crate) struct Assembler {
    buf: String,
    too_big: bool,
}

impl Assembler {
    /// The whole text once its last part arrived.
    pub(crate) fn push(&mut self, part: Part) -> Result<Option<String>, String> {
        if !self.too_big {
            if self.buf.len() + part.text.len() > MAX_CLIPBOARD_BYTES {
                self.too_big = true;
                self.buf = String::new();
            } else {
                self.buf.push_str(&part.text);
            }
        }
        if part.more {
            return Ok(None);
        }
        let text = std::mem::take(&mut self.buf);
        if std::mem::take(&mut self.too_big) {
            return Err(format!(
                "text is larger than {} KiB",
                MAX_CLIPBOARD_BYTES / 1024
            ));
        }
        Ok(Some(text))
    }
}

#[derive(Serialize)]
struct TextOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    more: bool,
}

/// The messages that carry `text`.
pub(crate) fn text_messages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let cut = rest
            .char_indices()
            .nth(CHUNK_CHARS)
            .map_or(rest.len(), |(i, _)| i);
        let (part, tail) = rest.split_at(cut);
        let more = !tail.is_empty();
        out.push(
            serde_json::to_string(&TextOut {
                kind: "clipboard",
                text: part,
                more,
            })
            .unwrap(),
        );
        if !more {
            return out;
        }
        rest = tail;
    }
}

fn status_message(s: &ClipboardState) -> String {
    serde_json::json!({
        "type": "clipboard_status",
        "available": s.available,
        "backend": s.backend,
        "reason": s.reason,
    })
    .to_string()
}

/// Sends the viewer the clipboard status now, then every status change and every new host
/// clipboard text, until the channel closes.
pub(crate) async fn forward_to_viewer(
    id: SessionId,
    dc: Arc<RTCDataChannel>,
    mut clipboard: ClipboardWatch,
) {
    let mut last = clipboard.borrow_and_update().clone();
    if dc.send_text(status_message(&last)).await.is_err() {
        return;
    }
    while clipboard.changed().await.is_ok() {
        let now = clipboard.borrow_and_update().clone();
        if (now.available, &now.reason) != (last.available, &last.reason) {
            if dc.send_text(status_message(&now)).await.is_err() {
                return;
            }
        }
        if now.serial != last.serial {
            if let Some(text) = &now.text {
                if text.len() <= MAX_CLIPBOARD_BYTES {
                    for m in text_messages(text) {
                        if let Err(e) = dc.send_text(m).await {
                            tracing::debug!(session = id, "clipboard to viewer: {e}");
                            return;
                        }
                    }
                }
            }
        }
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(json: &str) -> Part {
        parse_part(json.as_bytes()).expect("a clipboard part")
    }

    #[test]
    fn input_events_are_not_clipboard_parts() {
        assert!(parse_part(br#"{"type":"key","code":"KeyV","down":true}"#).is_none());
        assert!(parse_part(br#"{"type":"clipboard_status","available":true}"#).is_none());
    }

    #[test]
    fn split_and_join_roundtrip() {
        let text: String = "ab€😀\n\"".repeat(CHUNK_CHARS / 2);
        let msgs = text_messages(&text);
        assert!(msgs.len() > 1);
        assert!(msgs.iter().all(|m| m.len() < 60 * 1024));
        let mut asm = Assembler::default();
        let mut out = None;
        for (i, m) in msgs.iter().enumerate() {
            let r = asm.push(part(m)).unwrap();
            assert_eq!(r.is_some(), i == msgs.len() - 1);
            out = r.or(out);
        }
        assert_eq!(out.as_deref(), Some(text.as_str()));
        assert_eq!(
            text_messages(""),
            vec![r#"{"type":"clipboard","text":"","more":false}"#.to_string()]
        );
    }

    #[test]
    fn oversized_text_is_refused_and_the_next_one_works() {
        let mut asm = Assembler::default();
        let big = "x".repeat(CHUNK_CHARS);
        let parts = MAX_CLIPBOARD_BYTES / CHUNK_CHARS + 1;
        for _ in 0..parts {
            assert_eq!(
                asm.push(Part {
                    kind: "clipboard".into(),
                    text: big.clone(),
                    more: true
                })
                .unwrap(),
                None
            );
        }
        assert!(asm
            .push(part(r#"{"type":"clipboard","text":"end"}"#))
            .is_err());
        assert_eq!(
            asm.push(part(r#"{"type":"clipboard","text":"ok"}"#))
                .unwrap()
                .as_deref(),
            Some("ok")
        );
    }
}
