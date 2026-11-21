//! CASTV2 messages on the wire: each is a 4-byte big-endian length, then a `CastMessage` in
//! protobuf. Only text payloads are sent or kept; the JSON inside says the rest.
//!
//! The fields, from Chromium's `cast_channel.proto`: 1 protocol_version (0, CASTV2_1_0),
//! 2 source_id, 3 destination_id, 4 namespace, 5 payload_type (0, STRING), 6 payload_utf8, and
//! 7 payload_binary, which newsreel never sends.

use std::io;

/// The largest message body a receiver sends or takes, as Chromium's Open Screen sets it.
pub const MOST: usize = 64 * 1024;

/// A message with a text payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

impl Message {
    /// The message with its length before it, ready to send.
    pub fn framed(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64 + self.payload.len());
        varint_field(&mut body, 1, 0);
        text_field(&mut body, 2, &self.source);
        text_field(&mut body, 3, &self.destination);
        text_field(&mut body, 4, &self.namespace);
        varint_field(&mut body, 5, 0);
        text_field(&mut body, 6, &self.payload);
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend(body);
        framed
    }

    /// Reads one message body. A binary payload reads as an empty text one, which no namespace
    /// newsreel speaks sends.
    pub fn parse(body: &[u8]) -> io::Result<Message> {
        let mut message = Message {
            source: String::new(),
            destination: String::new(),
            namespace: String::new(),
            payload: String::new(),
        };
        let mut at = 0;
        while at < body.len() {
            let key = varint(body, &mut at)?;
            let (field, kind) = (key >> 3, key & 7);
            match kind {
                0 => {
                    varint(body, &mut at)?;
                }
                2 => {
                    let length = usize::try_from(varint(body, &mut at)?).map_err(|_| bad("a field is too long"))?;
                    let end = at
                        .checked_add(length)
                        .filter(|end| *end <= body.len())
                        .ok_or_else(|| bad("a field runs past the message"))?;
                    let bytes = &body[at..end];
                    at = end;
                    let text = || String::from_utf8(bytes.to_vec()).map_err(|_| bad("a text field is not UTF-8"));
                    match field {
                        2 => message.source = text()?,
                        3 => message.destination = text()?,
                        4 => message.namespace = text()?,
                        6 => message.payload = text()?,
                        _ => {}
                    }
                }
                1 => at = skip(body, at, 8)?,
                5 => at = skip(body, at, 4)?,
                _ => return Err(bad("a field has a wire type protobuf does not use")),
            }
        }
        Ok(message)
    }
}

/// Splits the whole messages off the front of `buffer`. A length of nothing, or past `MOST`,
/// means the stream is out of step, which only closing it mends.
pub fn split(buffer: &mut Vec<u8>) -> io::Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut at = 0;
    while let Some(head) = buffer.get(at..at + 4) {
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if length == 0 || length > MOST {
            return Err(bad(&format!("a message says it is {length} bytes long")));
        }
        let Some(body) = buffer.get(at + 4..at + 4 + length) else { break };
        messages.push(Message::parse(body)?);
        at += 4 + length;
    }
    buffer.drain(..at);
    Ok(messages)
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the Cast device sent a message newsreel cannot read: {what}"))
}

fn skip(body: &[u8], at: usize, length: usize) -> io::Result<usize> {
    at.checked_add(length).filter(|end| *end <= body.len()).ok_or_else(|| bad("a field runs past the message"))
}

fn varint(body: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *body.get(*at).ok_or_else(|| bad("a number runs past the message"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(bad("a number is longer than 64 bits"))
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    put_varint(out, field << 3);
    put_varint(out, value);
}

fn text_field(out: &mut Vec<u8>, field: u64, text: &str) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(payload: &str) -> Message {
        Message {
            source: "sender-0".into(),
            destination: "receiver-0".into(),
            namespace: "urn:x-cast:com.google.cast.tp.heartbeat".into(),
            payload: payload.into(),
        }
    }

    #[test]
    fn a_message_is_its_length_then_its_fields_in_protobuf() {
        let framed = message(r#"{"type":"PING"}"#).framed();
        // Version 0, then "sender-0" as field 2, as Chromium's encoder writes them.
        assert_eq!(&framed[4..16], b"\x08\x00\x12\x08sender-0");
        assert_eq!(u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize, framed.len() - 4);
        assert_eq!(Message::parse(&framed[4..]).unwrap(), message(r#"{"type":"PING"}"#));
    }

    #[test]
    fn messages_split_where_their_lengths_say_and_wait_for_the_rest() {
        let (one, two) = (message("1").framed(), message(&"x".repeat(300)).framed());
        let mut buffer = [one.clone(), two[..10].to_vec()].concat();
        assert_eq!(split(&mut buffer).unwrap(), [message("1")]);
        assert_eq!(buffer, two[..10]);
        buffer.extend_from_slice(&two[10..]);
        assert_eq!(split(&mut buffer).unwrap(), [message(&"x".repeat(300))]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_length_past_the_limit_or_a_broken_field_is_refused() {
        let mut huge = ((MOST + 1) as u32).to_be_bytes().to_vec();
        assert!(split(&mut huge).is_err());
        assert!(split(&mut vec![0, 0, 0, 0]).is_err());
        assert!(Message::parse(b"\x12\x09sender-0").is_err(), "a field nine bytes long holds eight");
        // A field this version does not know, such as a binary payload, is passed over.
        let mut known = message("{}").framed()[4..].to_vec();
        known.extend_from_slice(b"\x3a\x02\x01\x02\x40\x05");
        assert_eq!(Message::parse(&known).unwrap(), message("{}"));
    }
}
