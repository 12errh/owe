//! Newline-delimited JSON framing with a hard size cap.
//!
//! The decoder is *incremental*: bytes arrive in arbitrary chunks and complete
//! lines are yielded one at a time. When the pending tail exceeds
//! [`MAX_FRAME_BYTES`] the stream cannot be resynchronised, so the decoder
//! becomes **poisoned** and every later call fails: the caller's only sane
//! reaction is to send one final error frame and close.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Maximum size of one frame, including its terminating newline.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Framing errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    /// The frame exceeded [`MAX_FRAME_BYTES`]; the stream cannot be resynchronised.
    #[error("frame exceeds the {MAX_FRAME_BYTES} byte limit; closing connection")]
    TooLarge,

    /// The payload was a complete line but not a valid message.
    #[error("malformed frame: {0}")]
    Malformed(String),

    /// The decoder is poisoned after a fatal framing error.
    #[error("frame decoder is poisoned after a fatal framing error")]
    Poisoned,
}

/// Encode one message as a JSON line (newline included).
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|error| FrameError::Malformed(error.to_string()))?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode one message from a frame payload (a line without its newline).
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(payload).map_err(|error| FrameError::Malformed(error.to_string()))
}

/// Incremental line decoder with a hard size cap.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    poisoned: bool,
}

impl FrameDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes received from the socket.
    ///
    /// Never fails for size reasons on its own: size violations surface from
    /// [`FrameDecoder::next_frame`], which is where a whole line is evaluated.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Yield the next complete frame payload, if one is buffered.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }

        let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') else {
            if self.buffer.len() > MAX_FRAME_BYTES {
                self.poisoned = true;
                return Err(FrameError::TooLarge);
            }
            return Ok(None);
        };

        if position > MAX_FRAME_BYTES {
            self.poisoned = true;
            return Err(FrameError::TooLarge);
        }

        let line: Vec<u8> = self.buffer.drain(..=position).collect();
        Ok(Some(line[..line.len() - 1].to_vec()))
    }

    /// Bytes currently buffered but not yet yielded.
    pub fn pending_len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether a fatal framing error has occurred.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_one_line_per_message() {
        let bytes = encode(&serde_json::json!({ "v": 1, "id": "c1" })).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
    }

    #[test]
    fn round_trips_a_message() {
        let value = serde_json::json!({ "v": 1, "id": "c1", "method": "hello" });
        let bytes = encode(&value).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes).unwrap();
        let payload = decoder.next_frame().unwrap().expect("one frame");
        let decoded: serde_json::Value = decode(&payload).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn splits_multiple_frames_from_one_chunk() {
        let mut chunk = encode(&serde_json::json!({ "n": 1 })).unwrap();
        chunk.extend(encode(&serde_json::json!({ "n": 2 })).unwrap());
        chunk.extend(encode(&serde_json::json!({ "n": 3 })).unwrap());

        let mut decoder = FrameDecoder::new();
        decoder.push(&chunk).unwrap();
        let mut seen = Vec::new();
        while let Some(payload) = decoder.next_frame().unwrap() {
            let value: serde_json::Value = decode(&payload).unwrap();
            seen.push(value["n"].as_u64().unwrap());
        }
        assert_eq!(seen, vec![1, 2, 3]);
        assert_eq!(decoder.pending_len(), 0);
    }

    #[test]
    fn waits_for_the_rest_of_a_partial_frame() {
        let bytes = encode(&serde_json::json!({ "v": 1, "id": "c1" })).unwrap();
        let (head, tail) = bytes.split_at(5);

        let mut decoder = FrameDecoder::new();
        decoder.push(head).unwrap();
        assert!(decoder.next_frame().unwrap().is_none(), "no newline yet");
        assert_eq!(decoder.pending_len(), 5);

        decoder.push(tail).unwrap();
        let payload = decoder.next_frame().unwrap().expect("frame after tail");
        let value: serde_json::Value = decode(&payload).unwrap();
        assert_eq!(value["id"], "c1");
    }

    #[test]
    fn oversize_frame_without_newline_poisons_the_decoder() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&vec![b'x'; MAX_FRAME_BYTES + 1]).unwrap();

        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::TooLarge);
        assert!(decoder.is_poisoned());
        assert_eq!(decoder.push(b"more").unwrap_err(), FrameError::Poisoned);
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Poisoned);
    }

    #[test]
    fn single_oversize_line_is_rejected_when_the_newline_arrives() {
        let mut blob = vec![b'x'; MAX_FRAME_BYTES + 10];
        blob.push(b'\n');

        let mut decoder = FrameDecoder::new();
        decoder.push(&blob).unwrap();
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::TooLarge);
    }

    #[test]
    fn malformed_json_does_not_poison_the_decoder() {
        let mut chunk = b"not json at all\n".to_vec();
        chunk.extend(encode(&serde_json::json!({ "v": 1, "id": "after" })).unwrap());

        let mut decoder = FrameDecoder::new();
        decoder.push(&chunk).unwrap();

        let first = decoder.next_frame().unwrap().expect("first frame");
        let err = decode::<serde_json::Value>(&first).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)), "{err:?}");
        // Recoverable: the next frame is still available (TRD FR-CORE-6).
        assert!(!decoder.is_poisoned());
        let second = decoder.next_frame().unwrap().expect("second frame");
        let value: serde_json::Value = decode(&second).unwrap();
        assert_eq!(value["id"], "after");
    }

    #[test]
    fn empty_line_yields_an_empty_payload() {
        let mut decoder = FrameDecoder::new();
        decoder.push(b"\n").unwrap();
        let payload = decoder.next_frame().unwrap().expect("empty frame");
        assert!(payload.is_empty());
        assert!(matches!(
            decode::<serde_json::Value>(&payload),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn encode_refuses_messages_over_the_cap() {
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        let err = encode(&serde_json::json!({ "blob": huge })).unwrap_err();
        assert_eq!(err, FrameError::TooLarge);
    }
}
