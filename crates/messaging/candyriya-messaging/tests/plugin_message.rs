//! `PluginMessage` / `PluginMessageBuilder` / `MessageBuffer` behavior.

use bytes::Bytes;

use candyriya_messaging::java::{read_java_int, read_java_string};
use candyriya_messaging::{ChannelIdentifier, MessageBuffer, PluginMessage, PluginMessageBuilder};

#[test]
fn message_carries_arbitrary_bytes() {
    let channel = ChannelIdentifier::new("bungeecord:main").unwrap();
    let payload = Bytes::from_static(&[0x00, 0xFF, 0x7F, 0x80, 0xC0, 0x80]);
    let message = PluginMessage::new(channel.clone(), payload.clone());
    assert_eq!(message.channel, channel);
    assert_eq!(message.data, payload);

    // empty payload is legal: some subchannels carry an empty body
    let message = PluginMessage::new(channel.clone(), Vec::new());
    assert!(message.data.is_empty());
}

#[test]
fn builder_assembles_payload_in_wire_order() {
    let channel = ChannelIdentifier::new("example:main").unwrap();
    let mut builder = PluginMessageBuilder::new(channel);
    assert!(builder.is_empty());
    builder.write_java_string("cmd").unwrap();
    builder.write_java_int(-7);
    builder.write_java_bool(true);
    builder.write_java_i64(9_000_000_000);

    assert_eq!(builder.channel().as_str(), "example:main");
    assert!(!builder.is_empty());

    let message = builder.build();
    let mut cursor = message.data.as_ref();
    assert_eq!(read_java_string(&mut cursor, 64).unwrap(), "cmd");
    assert_eq!(read_java_int(&mut cursor).unwrap(), -7);
    assert!(candyriya_messaging::java::read_java_bool(&mut cursor).unwrap());
    assert_eq!(
        candyriya_messaging::java::read_java_i64(&mut cursor).unwrap(),
        9_000_000_000
    );
    assert!(cursor.is_empty());
}

#[test]
fn builder_clear_reuses_capacity_and_channel() {
    let channel = ChannelIdentifier::new("example:main").unwrap();
    let mut builder = PluginMessageBuilder::with_capacity(channel, 128);
    builder.write_java_string("first").unwrap();
    builder.clear();
    assert!(builder.is_empty());
    builder.write_java_string("second").unwrap();
    let message = builder.build();
    let mut cursor = message.data.as_ref();
    assert_eq!(read_java_string(&mut cursor, 64).unwrap(), "second");
}

#[test]
fn message_buffer_into_bytes_and_reuse() {
    let mut buf = MessageBuffer::with_capacity(64);
    buf.write_bytes(&[1, 2, 3]);
    buf.write_java_u8(0x04);
    buf.write_java_u16(300);
    assert_eq!(buf.as_slice(), &[1, 2, 3, 0x04, 0x01, 0x2C]);
    assert_eq!(buf.len(), 6);

    let bytes = buf.into_bytes();
    assert_eq!(&bytes[..], &[1, 2, 3, 0x04, 0x01, 0x2C]);

    let mut buf = MessageBuffer::new();
    buf.write_bytes(&[9]);
    buf.clear();
    assert_eq!(buf.len(), 0);
    buf.write_java_bool(true);
    assert_eq!(buf.as_slice(), &[0x01]);
}

#[test]
fn write_java_string_error_does_not_corrupt_buffer_reuse() {
    let mut buf = MessageBuffer::new();
    buf.write_java_string("ok").unwrap();
    let err = buf.write_java_string(&"x".repeat(65536)).unwrap_err();
    assert!(matches!(
        err,
        candyriya_messaging::JavaCodecError::EncodedStringTooLong { .. }
    ));
    // the failed write must not have appended the prefix or payload
    assert_eq!(buf.as_slice().len(), 2 + "ok".len());
    buf.clear();
    assert_eq!(buf.len(), 0);
}

#[test]
fn plugin_message_is_cloneable_and_comparable() {
    let channel = ChannelIdentifier::new("a:b").unwrap();
    let a = PluginMessage::new(channel.clone(), Bytes::from_static(&[1]));
    let b = a.clone();
    assert_eq!(a, b);
}
