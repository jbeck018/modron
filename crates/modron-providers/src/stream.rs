//! SSE (server-sent-events) wire parsing — LEDGER item S1.
//!
//! A hand-rolled, dependency-free frame parser shared by both provider
//! streaming implementations, plus the [`StreamDelta`] type the providers
//! push to callers while a turn is in flight. The parser is a pure function
//! over a caller-owned buffer so that events split across TCP chunk
//! boundaries are handled by construction (see [`parse_sse_stream`]).
//!
//! Nothing here interprets provider semantics: `data: [DONE]` is returned as
//! the payload `"[DONE]"` and each provider applies its own sentinel and
//! JSON decoding on top.

/// One incremental piece of a model turn, emitted while the stream arrives.
///
/// Deltas are strictly ordered; `Done` is the final item and is only sent on
/// success (an error return means the stream never completed).
/// Streaming deltas — defined in `modron-core` (the trait carries them);
/// re-exported here as the natural import path for provider users.
pub use modron_core::StreamDelta;

/// Parse buffered SSE bytes, returning every *complete* `data:` payload plus
/// the number of bytes consumed from the front of the buffer.
///
/// Buffered state lives with the caller:
///
/// 1. append the new network chunk to a persistent `Vec<u8>`,
/// 2. call this on the whole buffer,
/// 3. drop the first `consumed` bytes and keep the tail for the next chunk.
///
/// Because `consumed` never covers a partial event, events split across
/// chunk boundaries are re-assembled transparently — including a multi-byte
/// UTF-8 character cut in half (the tail is raw bytes, not a lossy string).
///
/// Framing rules implemented (per the SSE spec, as spoken by both providers):
/// - an event is a run of field lines terminated by a blank line;
/// - all `data:` lines of one event are joined with `\n` into one payload
///   (SSE multiline data);
/// - `event:`, `id:`, `retry:` fields and `: comment` lines are parsed and
///   ignored;
/// - a `data` line with no colon contributes an empty value; only one
///   leading space after the colon is stripped (`data:  x` yields `" x"`);
/// - both LF and CRLF terminate lines; a lone mid-buffer CR also terminates;
///   a trailing CR at the end of the buffer is held back because it may be
///   the first half of a CRLF that has not arrived yet;
/// - blank lines between events (keep-alive pings) are consumed without
///   producing a payload, so the buffer cannot grow unboundedly.
pub fn parse_sse_stream(chunk: &[u8]) -> (Vec<String>, usize) {
    let mut payloads: Vec<String> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut consumed = 0usize;
    let mut pos = 0usize;

    while pos < chunk.len() {
        let Some((line_end, next_pos)) = find_line_end(chunk, pos) else {
            break;
        };
        let line = &chunk[pos..line_end];
        if line.is_empty() {
            // Blank line: end of event. Emit accumulated data (if any) and
            // consume through the blank line either way — keep-alive pings
            // must not accumulate.
            if !data_lines.is_empty() {
                payloads.push(data_lines.join("\n"));
                data_lines.clear();
            }
            consumed = next_pos;
        } else if line[0] == b':' {
            // Comment line — ignored, but still part of the event frame.
        } else {
            let (field, value) = split_field(line);
            if field == b"data" {
                data_lines.push(String::from_utf8_lossy(value).into_owned());
            }
            // event:/id:/retry:/unknown fields are intentionally ignored.
        }
        pos = next_pos;
    }
    (payloads, consumed)
}

/// Find the next line terminator at or after `pos`. Returns
/// `(index_of_terminator_start, position_after_terminator)`, or `None` when
/// the buffer holds no complete line. `\n`, `\r\n` and a mid-buffer lone
/// `\r` all terminate; a trailing `\r` at the very end of the buffer does
/// not (it may precede an LF that has not arrived yet).
fn find_line_end(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    let mut i = pos;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return Some((i, i + 1)),
            b'\r' => {
                return if i + 1 < bytes.len() {
                    let after = if bytes[i + 1] == b'\n' { i + 2 } else { i + 1 };
                    Some((i, after))
                } else {
                    None
                };
            }
            _ => i += 1,
        }
    }
    None
}

/// Split `field: value` at the first colon. A line without a colon is a
/// field with an empty value; a single leading space after the colon is
/// stripped (SSE §"field value").
fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        Some(colon) => {
            let mut value = &line[colon + 1..];
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            (&line[..colon], value)
        }
        None => (line, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunk` through the caller-buffered contract the providers use:
    /// keep the unconsumed tail, append the next chunk, repeat.
    fn feed(buffer: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
        buffer.extend_from_slice(chunk);
        let (payloads, consumed) = parse_sse_stream(buffer);
        buffer.drain(..consumed);
        payloads
    }

    #[test]
    fn single_event_with_lf() {
        let (payloads, consumed) = parse_sse_stream(b"data: hello\n\n");
        assert_eq!(payloads, vec!["hello"]);
        assert_eq!(consumed, 13);
    }

    #[test]
    fn single_event_with_crlf() {
        let (payloads, consumed) = parse_sse_stream(b"data: hello\r\n\r\n");
        assert_eq!(payloads, vec!["hello"]);
        assert_eq!(consumed, 15);
    }

    #[test]
    fn event_line_and_comments_are_ignored() {
        let raw = b"event: message_start\n: keep-alive\ndata: {\"a\":1}\n\n";
        let (payloads, consumed) = parse_sse_stream(raw);
        assert_eq!(payloads, vec![r#"{"a":1}"#]);
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn multiple_events_in_one_chunk_in_order() {
        let raw = b"data: one\n\ndata: two\n\n";
        let (payloads, consumed) = parse_sse_stream(raw);
        assert_eq!(payloads, vec!["one", "two"]);
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let raw = b"data: line1\ndata: line2\n\n";
        let (payloads, _) = parse_sse_stream(raw);
        assert_eq!(payloads, vec!["line1\nline2"]);
    }

    #[test]
    fn done_sentinel_passes_through_verbatim() {
        // The parser is provider-agnostic: [DONE] is an ordinary payload.
        let (payloads, _) = parse_sse_stream(b"data: [DONE]\n\n");
        assert_eq!(payloads, vec!["[DONE]"]);
    }

    #[test]
    fn incomplete_trailing_event_is_not_consumed_or_emitted() {
        let (payloads, consumed) = parse_sse_stream(b"data: a\n\ndata: par");
        assert_eq!(payloads, vec!["a"]);
        assert_eq!(
            consumed, 9,
            "only complete events (incl. their blank line) are consumed"
        );
    }

    #[test]
    fn incomplete_data_line_is_held_back() {
        // A complete line but no blank line yet: the event is still open.
        let (payloads, consumed) = parse_sse_stream(b"data: half\n");
        assert!(payloads.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn blank_keepalive_lines_are_consumed_without_payloads() {
        let (payloads, consumed) = parse_sse_stream(b"\n\n");
        assert!(payloads.is_empty());
        assert_eq!(consumed, 2);
    }

    #[test]
    fn data_without_colon_is_empty_payload() {
        let (payloads, _) = parse_sse_stream(b"data\n\n");
        assert_eq!(payloads, vec![""]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let (payloads, _) = parse_sse_stream(b"data:  spaced\n\n");
        assert_eq!(payloads, vec![" spaced"]);
    }

    #[test]
    fn trailing_cr_is_ambiguous_and_held_back() {
        // "data: hi\r" could become "data: hi\r\n..." — wait for more bytes.
        let (payloads, consumed) = parse_sse_stream(b"data: hi\r");
        assert!(payloads.is_empty());
        assert_eq!(consumed, 0);
        // And the completed CRLF form parses.
        let (payloads, _) = parse_sse_stream(b"data: hi\r\n\r\n");
        assert_eq!(payloads, vec!["hi"]);
    }

    #[test]
    fn event_split_across_chunks_via_caller_buffer() {
        let mut buffer: Vec<u8> = Vec::new();
        assert!(feed(&mut buffer, b"data: he").is_empty());
        assert!(feed(&mut buffer, b"ll").is_empty());
        let payloads = feed(&mut buffer, b"o\n\ndata: next\n\n");
        assert_eq!(payloads, vec!["hello", "next"]);
        assert!(buffer.is_empty(), "fully consumed buffer leaves no tail");
    }

    #[test]
    fn multibyte_character_split_across_chunks() {
        let word = "héllo wörld"; // é and ö are two bytes each
        let bytes = word.as_bytes();
        let mut buffer: Vec<u8> = Vec::new();
        let mut collected = Vec::new();
        // Cut one byte into the final two-byte 'ö'.
        collected.extend(feed(&mut buffer, b"data: "));
        collected.extend(feed(&mut buffer, &bytes[..bytes.len() - 1]));
        collected.extend(feed(&mut buffer, &bytes[bytes.len() - 1..]));
        collected.extend(feed(&mut buffer, b"\n\n"));
        assert_eq!(collected, vec![word]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn byte_by_byte_feeding_matches_one_shot() {
        let lines = [
            "event: content_block_delta",
            "data: {\"partial_json\":\"{\\\"pa\"}", // JSON: {"partial_json":"{\"pa"}
            "",
            ":comment",
            "data: second",
            "",
            "data: [DONE]",
            "",
        ];
        let expected = vec![r#"{"partial_json":"{\"pa"}"#, "second", "[DONE]"];
        // Prove it for both LF and CRLF framing.
        for terminator in ["\n", "\r\n"] {
            // The trailing empty element ends the last data line; one more
            // terminator adds the blank line that dispatches the event.
            let body = format!("{}{}", lines.join(terminator), terminator);
            let bytes = body.as_bytes();
            let (one_shot, _) = parse_sse_stream(bytes);

            let mut buffer: Vec<u8> = Vec::new();
            let mut collected = Vec::new();
            for byte in bytes {
                collected.extend(feed(&mut buffer, std::slice::from_ref(byte)));
            }
            assert_eq!(
                collected, one_shot,
                "byte-by-byte must match one-shot ({terminator:?})"
            );
            assert_eq!(
                collected, expected,
                "payloads must survive byte-wise arrival"
            );
            assert!(buffer.is_empty());
        }
    }
}
