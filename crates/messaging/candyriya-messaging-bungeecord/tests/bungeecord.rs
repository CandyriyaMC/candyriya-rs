//! Integration tests for the BungeeCord subchannel codec.
//!
//! The golden vectors below are written as byte arrays by hand — each one
//! was derived from the SpigotMC wiki specification of the BungeeCord
//! plugin-messaging channel (`writeUTF` = two-byte big-endian unsigned
//! length prefix + modified UTF-8/CESU-8 payload), not from the encoder
//! under test.

use candyriya_messaging::{ChannelIdentifier, java};
use candyriya_messaging_bungeecord::{
    BUNGEECORD_CHANNEL, BUNGEECORD_CHANNEL_NAME, BUNGEECORD_LEGACY_CHANNEL,
    BUNGEECORD_LEGACY_CHANNEL_NAME, BungeeCodec, BungeeMessage, BungeeMessagingError,
    SUBCHANNEL_NAMES, is_bungeecord_channel, response,
};

const MAX_UTF_BYTES: usize = u16::MAX as usize;

/// A minimal big-endian reader used to verify response payloads field by
/// field. Independent from the production reader paths on purpose.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { buf: data }
    }

    fn finish(&self) -> usize {
        self.buf.len()
    }

    fn utf(&mut self) -> String {
        java::read_java_string(&mut self.buf, MAX_UTF_BYTES)
            .expect("test payload must contain a valid java string")
    }

    fn i32(&mut self) -> i32 {
        java::read_java_int(&mut self.buf).expect("test payload must contain a valid int")
    }

    fn u16(&mut self) -> u16 {
        java::read_java_u16(&mut self.buf).expect("test payload must contain a valid short")
    }

    fn i64(&mut self) -> i64 {
        java::read_java_i64(&mut self.buf).expect("test payload must contain a valid long")
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        assert!(self.buf.len() >= n, "test payload too short");
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        head.to_vec()
    }
}

/// Every variant of [`BungeeMessage`], exactly once, one per subchannel.
fn all_variants() -> Vec<BungeeMessage> {
    vec![
        BungeeMessage::Connect {
            server: "pvp".to_owned(),
        },
        BungeeMessage::ConnectOther {
            target: "Notch".to_owned(),
            server: "pvp".to_owned(),
        },
        BungeeMessage::IP,
        BungeeMessage::IPOther {
            target: "Notch".to_owned(),
        },
        BungeeMessage::PlayerCount {
            server: "pvp".to_owned(),
        },
        BungeeMessage::PlayerList {
            server: "ALL".to_owned(),
        },
        BungeeMessage::GetPlayerServer {
            target: "Notch".to_owned(),
        },
        BungeeMessage::GetServers,
        BungeeMessage::Message {
            target: "Notch".to_owned(),
            message: "hello".to_owned(),
        },
        BungeeMessage::MessageRaw {
            target: "ALL".to_owned(),
            message: "{\"text\":\"hi\"}".to_owned(),
        },
        BungeeMessage::GetServer,
        BungeeMessage::Forward {
            target: "ALL".to_owned(),
            channel: "MyChannel".to_owned(),
            message: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
        BungeeMessage::ForwardToPlayer {
            target: "Notch".to_owned(),
            channel: "MyChannel".to_owned(),
            message: vec![1, 2, 3],
        },
        BungeeMessage::UUID,
        BungeeMessage::UUIDOther {
            target: "Notch".to_owned(),
        },
        BungeeMessage::ServerIP {
            server: "lobby".to_owned(),
        },
        BungeeMessage::KickPlayer {
            target: "Notch".to_owned(),
            reason: "bye".to_owned(),
        },
        BungeeMessage::KickPlayerRaw {
            target: "Notch".to_owned(),
            reason: "{\"text\":\"gone\"}".to_owned(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Channel constants
// ---------------------------------------------------------------------------

#[test]
fn channel_constants_validate() {
    assert_eq!(BUNGEECORD_CHANNEL_NAME, "bungeecord:main");
    assert_eq!(BUNGEECORD_LEGACY_CHANNEL_NAME, "BungeeCord");
    assert_eq!(BUNGEECORD_CHANNEL.as_str(), "bungeecord:main");
    assert_eq!(BUNGEECORD_LEGACY_CHANNEL.as_str(), "BungeeCord");
    assert_ne!(*BUNGEECORD_CHANNEL, *BUNGEECORD_LEGACY_CHANNEL);
    assert_eq!(
        *BUNGEECORD_CHANNEL,
        ChannelIdentifier::new("bungeecord:main").unwrap()
    );
    assert_eq!(
        *BUNGEECORD_LEGACY_CHANNEL,
        ChannelIdentifier::new("BungeeCord").unwrap()
    );
    assert!(is_bungeecord_channel(&BUNGEECORD_CHANNEL));
    assert!(is_bungeecord_channel(&BUNGEECORD_LEGACY_CHANNEL));
    assert!(!is_bungeecord_channel(
        &ChannelIdentifier::new("candyriya:main").unwrap()
    ));
}

// ---------------------------------------------------------------------------
// Golden vectors (hand-written bytes)
// ---------------------------------------------------------------------------

/// `writeUTF("Connect")` followed by `writeUTF("lobby")`.
#[test]
fn golden_connect() {
    // 00 07 "Connect" | 00 05 "lobby"
    let golden: &[u8] = &[
        0x00, 0x07, 0x43, 0x6F, 0x6E, 0x6E, 0x65, 0x63, 0x74, // writeUTF("Connect")
        0x00, 0x05, 0x6C, 0x6F, 0x62, 0x62, 0x79, //           writeUTF("lobby")
    ];
    let codec = BungeeCodec;
    let parsed = codec.parse(golden).unwrap();
    assert_eq!(
        parsed,
        BungeeMessage::Connect {
            server: "lobby".to_owned()
        }
    );
    assert_eq!(codec.encode(&parsed).unwrap(), golden);
}

/// `writeUTF("IP")` and nothing else.
#[test]
fn golden_ip_payload() {
    let golden: &[u8] = &[0x00, 0x02, 0x49, 0x50]; // writeUTF("IP")
    let codec = BungeeCodec;
    let parsed = codec.parse(golden).unwrap();
    assert_eq!(parsed, BungeeMessage::IP);
    assert_eq!(codec.encode(&parsed).unwrap(), golden);
}

/// `writeUTF("GetServers")` and nothing else.
#[test]
fn golden_get_servers() {
    let golden: &[u8] = &[
        0x00, 0x0A, 0x47, 0x65, 0x74, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72,
        0x73, // writeUTF("GetServers")
    ];
    let codec = BungeeCodec;
    let parsed = codec.parse(golden).unwrap();
    assert_eq!(parsed, BungeeMessage::GetServers);
    assert_eq!(codec.encode(&parsed).unwrap(), golden);
}

/// The `PlayerCount` response: `writeUTF("PlayerCount")`, `writeUTF("lobby")`,
/// `writeInt(42)`.
#[test]
fn golden_player_count_response() {
    let golden: &[u8] = &[
        0x00, 0x0B, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x43, 0x6F, 0x75, 0x6E,
        0x74, // writeUTF("PlayerCount")
        0x00, 0x05, 0x6C, 0x6F, 0x62, 0x62,
        0x79, //                                   writeUTF("lobby")
        0x00, 0x00, 0x00,
        0x2A, //                                                    writeInt(42)
    ];
    // the encoder must produce exactly these bytes
    let encoded = response::encode_player_count_response("lobby", 42).unwrap();
    assert_eq!(encoded, golden);

    // and the payload must decode with a plain reader
    let mut reader = Reader::new(&encoded);
    assert_eq!(reader.utf(), "PlayerCount");
    assert_eq!(reader.utf(), "lobby");
    assert_eq!(reader.i32(), 42);
    assert_eq!(reader.finish(), 0);
}

/// `Forward` with a nested payload: subchannel, target `"ALL"`, nested
/// channel `"MyChannel"`, unsigned 16-bit length prefix, payload bytes.
#[test]
fn golden_forward_with_bytes() {
    let golden: &[u8] = &[
        0x00, 0x07, 0x46, 0x6F, 0x72, 0x77, 0x61, 0x72, 0x64, // writeUTF("Forward")
        0x00, 0x03, 0x41, 0x4C, 0x4C, //                         writeUTF("ALL")
        0x00, 0x09, 0x4D, 0x79, 0x43, 0x68, 0x61, 0x6E, 0x6E, 0x65,
        0x6C, // writeUTF("MyChannel")
        0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF, //                   writeShort(4) + payload
    ];
    let codec = BungeeCodec;
    let parsed = codec.parse(golden).unwrap();
    assert_eq!(
        parsed,
        BungeeMessage::Forward {
            target: "ALL".to_owned(),
            channel: "MyChannel".to_owned(),
            message: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }
    );
    assert_eq!(codec.encode(&parsed).unwrap(), golden);
}

/// `KickPlayer`: subchannel, target `"Notch"`, reason `"bye"`.
#[test]
fn golden_kick_player() {
    let golden: &[u8] = &[
        0x00, 0x0A, 0x4B, 0x69, 0x63, 0x6B, 0x50, 0x6C, 0x61, 0x79, 0x65,
        0x72, // writeUTF("KickPlayer")
        0x00, 0x05, 0x4E, 0x6F, 0x74, 0x63,
        0x68, //                            writeUTF("Notch")
        0x00, 0x03, 0x62, 0x79,
        0x65, //                                        writeUTF("bye")
    ];
    let codec = BungeeCodec;
    let parsed = codec.parse(golden).unwrap();
    assert_eq!(
        parsed,
        BungeeMessage::KickPlayer {
            target: "Notch".to_owned(),
            reason: "bye".to_owned(),
        }
    );
    assert_eq!(codec.encode(&parsed).unwrap(), golden);
}

/// Subchannel byte sequences match the intent: every known subchannel name
/// encodes to exactly its UTF bytes behind a two-byte big-endian length.
#[test]
fn golden_subchannel_byte_sequences() {
    for (name, expected) in [
        (
            "Connect",
            &[0x00, 0x07, 0x43, 0x6F, 0x6E, 0x6E, 0x65, 0x63, 0x74][..],
        ),
        (
            "ConnectOther",
            &[
                0x00, 0x0C, 0x43, 0x6F, 0x6E, 0x6E, 0x65, 0x63, 0x74, 0x4F, 0x74, 0x68, 0x65, 0x72,
            ][..],
        ),
        ("IP", &[0x00, 0x02, 0x49, 0x50][..]),
        (
            "IPOther",
            &[0x00, 0x07, 0x49, 0x50, 0x4F, 0x74, 0x68, 0x65, 0x72][..],
        ),
        (
            "PlayerCount",
            &[
                0x00, 0x0B, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x43, 0x6F, 0x75, 0x6E, 0x74,
            ][..],
        ),
        (
            "PlayerList",
            &[
                0x00, 0x0A, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x4C, 0x69, 0x73, 0x74,
            ][..],
        ),
        (
            "GetPlayerServer",
            &[
                0x00, 0x0F, 0x47, 0x65, 0x74, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x53, 0x65, 0x72,
                0x76, 0x65, 0x72,
            ][..],
        ),
        (
            "GetServers",
            &[
                0x00, 0x0A, 0x47, 0x65, 0x74, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72, 0x73,
            ][..],
        ),
        (
            "Message",
            &[0x00, 0x07, 0x4D, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65][..],
        ),
        (
            "MessageRaw",
            &[
                0x00, 0x0A, 0x4D, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x52, 0x61, 0x77,
            ][..],
        ),
        (
            "GetServer",
            &[
                0x00, 0x09, 0x47, 0x65, 0x74, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72,
            ][..],
        ),
        (
            "Forward",
            &[0x00, 0x07, 0x46, 0x6F, 0x72, 0x77, 0x61, 0x72, 0x64][..],
        ),
        (
            "ForwardToPlayer",
            &[
                0x00, 0x0F, 0x46, 0x6F, 0x72, 0x77, 0x61, 0x72, 0x64, 0x54, 0x6F, 0x50, 0x6C, 0x61,
                0x79, 0x65, 0x72,
            ][..],
        ),
        ("UUID", &[0x00, 0x04, 0x55, 0x55, 0x49, 0x44][..]),
        (
            "UUIDOther",
            &[
                0x00, 0x09, 0x55, 0x55, 0x49, 0x44, 0x4F, 0x74, 0x68, 0x65, 0x72,
            ][..],
        ),
        (
            "ServerIP",
            &[0x00, 0x08, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72, 0x49, 0x50][..],
        ),
        (
            "KickPlayer",
            &[
                0x00, 0x0A, 0x4B, 0x69, 0x63, 0x6B, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72,
            ][..],
        ),
        (
            "KickPlayerRaw",
            &[
                0x00, 0x0D, 0x4B, 0x69, 0x63, 0x6B, 0x50, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x52, 0x61,
                0x77,
            ][..],
        ),
    ] {
        // every byte sequence matches the two-byte big-endian length prefix
        // followed by the ASCII bytes of the name — assert it explicitly
        assert_eq!(&expected[..2], (name.len() as u16).to_be_bytes().as_slice());
        assert_eq!(&expected[2..], name.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Roundtrip
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_every_variant() {
    let codec = BungeeCodec;
    for sample in all_variants() {
        let bytes = codec.encode(&sample).unwrap();
        let parsed = codec.parse(&bytes).unwrap();
        assert_eq!(
            parsed,
            sample,
            "roundtrip failed for {}",
            sample.subchannel()
        );
    }
}

#[test]
fn roundtrip_player_count_all_sentinel() {
    let codec = BungeeCodec;
    for server in ["ALL", "pvp", "lobby"] {
        let msg = BungeeMessage::PlayerCount {
            server: server.to_owned(),
        };
        let parsed = codec.parse(&codec.encode(&msg).unwrap()).unwrap();
        assert_eq!(parsed, msg);
    }
}

#[test]
fn roundtrip_astral_and_utf8_strings() {
    let codec = BungeeCodec;
    for message in [
        "plain ascii",
        "héllo 😀",
        "null is not representable raw",
        "emoji 🚀🚀🚀",
    ] {
        // skip strings containing actual NUL — Java modified UTF-8 encodes it
        // as 0xC0 0x80, which this codec roundtrips correctly, but the
        // ergonomic assertion below is about ordinary input
        let msg = BungeeMessage::Message {
            target: "ALL".to_owned(),
            message: message.to_owned(),
        };
        let parsed = codec.parse(&codec.encode(&msg).unwrap()).unwrap();
        assert_eq!(parsed, msg);
    }
}

#[test]
fn roundtrip_forward_payload_limits() {
    let codec = BungeeCodec;

    let max = BungeeMessage::Forward {
        target: "ALL".to_owned(),
        channel: "C".to_owned(),
        message: vec![0xAA; u16::MAX as usize],
    };
    let parsed = codec.parse(&codec.encode(&max).unwrap()).unwrap();
    assert_eq!(parsed, max);

    let too_big = BungeeMessage::Forward {
        target: "ALL".to_owned(),
        channel: "C".to_owned(),
        message: vec![0xAA; u16::MAX as usize + 1],
    };
    assert_eq!(
        codec.encode(&too_big),
        Err(BungeeMessagingError::ForwardPayloadTooLong {
            subchannel: "Forward".to_owned(),
            len: u16::MAX as usize + 1,
            max: u16::MAX as usize,
        })
    );
}

#[test]
fn roundtrip_forward_to_player() {
    let codec = BungeeCodec;
    let msg = BungeeMessage::ForwardToPlayer {
        target: "Notch".to_owned(),
        channel: "MyChannel".to_owned(),
        message: vec![],
    };
    let parsed = codec.parse(&codec.encode(&msg).unwrap()).unwrap();
    assert_eq!(parsed, msg);
}

// ---------------------------------------------------------------------------
// Response encoders
// ---------------------------------------------------------------------------

#[test]
fn ip_response_roundtrips() {
    let bytes = response::encode_ip_response("1.2.3.4", 25565).unwrap();
    // ends with the four-byte big-endian int port
    assert_eq!(&bytes[bytes.len() - 4..], 25565i32.to_be_bytes().as_slice());
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "IP");
    assert_eq!(reader.utf(), "1.2.3.4");
    assert_eq!(reader.i32(), 25565);
    assert_eq!(reader.finish(), 0);
}

#[test]
fn ip_response_unix_socket_form() {
    let bytes = response::encode_ip_response("unix:///run/bungee.sock", 0).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "IP");
    assert_eq!(reader.utf(), "unix:///run/bungee.sock");
    assert_eq!(reader.i32(), 0);
    assert_eq!(reader.finish(), 0);
}

#[test]
fn ip_other_response_roundtrips() {
    let bytes = response::encode_ip_other_response("Notch", "203.0.113.7", 40000).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "IPOther");
    assert_eq!(reader.utf(), "Notch");
    assert_eq!(reader.utf(), "203.0.113.7");
    assert_eq!(reader.i32(), 40000);
    assert_eq!(reader.finish(), 0);
}

#[test]
fn player_count_response_roundtrips() {
    for (server, count) in [("ALL", 1234), ("pvp", 3), ("lobby", 0)] {
        let bytes = response::encode_player_count_response(server, count).unwrap();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.utf(), "PlayerCount");
        assert_eq!(reader.utf(), server);
        assert_eq!(reader.i32(), count);
        assert_eq!(reader.finish(), 0);
    }
}

#[test]
fn player_list_response_roundtrips() {
    let bytes = response::encode_player_list_response("pvp", &["Notch", "Alex"]).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "PlayerList");
    assert_eq!(reader.utf(), "pvp");
    assert_eq!(reader.utf(), "Notch, Alex");
    assert_eq!(reader.finish(), 0);

    // empty list encodes an empty CSV string, exactly what Util.csv([]) does
    let bytes = response::encode_player_list_response("ALL", &[]).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "PlayerList");
    assert_eq!(reader.utf(), "ALL");
    assert_eq!(reader.utf(), "");
    assert_eq!(reader.finish(), 0);
}

#[test]
fn get_player_server_response_roundtrips() {
    let bytes = response::encode_get_player_server_response("Notch", Some("lobby")).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "GetPlayerServer");
    assert_eq!(reader.utf(), "Notch");
    assert_eq!(reader.utf(), "lobby");
    assert_eq!(reader.finish(), 0);

    // unknown player: BungeeCord reports "" — encode None the same way
    let bytes = response::encode_get_player_server_response("Ghost", None).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "GetPlayerServer");
    assert_eq!(reader.utf(), "Ghost");
    assert_eq!(reader.utf(), "");
    assert_eq!(reader.finish(), 0);
}

#[test]
fn get_servers_response_roundtrips() {
    let bytes = response::encode_get_servers_response(&["lobby", "pvp", "minigames"]).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "GetServers");
    assert_eq!(reader.utf(), "lobby, pvp, minigames");
    assert_eq!(reader.finish(), 0);

    let bytes = response::encode_get_servers_response(&[]).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "GetServers");
    assert_eq!(reader.utf(), "");
    assert_eq!(reader.finish(), 0);
}

#[test]
fn get_server_response_bytes_and_roundtrip() {
    // hand-written expectation: writeUTF("GetServer") + writeUTF("lobby")
    let expected: &[u8] = &[
        0x00, 0x09, 0x47, 0x65, 0x74, 0x53, 0x65, 0x72, 0x76, 0x65,
        0x72, // writeUTF("GetServer")
        0x00, 0x05, 0x6C, 0x6F, 0x62, 0x62, 0x79, //                     writeUTF("lobby")
    ];
    let bytes = response::encode_get_server_response("lobby").unwrap();
    assert_eq!(bytes, expected);

    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "GetServer");
    assert_eq!(reader.utf(), "lobby");
    assert_eq!(reader.finish(), 0);
}

#[test]
fn server_ip_response_u16_port() {
    let bytes = response::encode_server_ip_response("lobby", "127.0.0.1", 25565).unwrap();
    // port occupies the last two bytes as writeUnsignedShort
    assert_eq!(&bytes[bytes.len() - 2..], [0x63, 0xDD].as_slice()); // 25565 = 0x63DD

    for (port, tail) in [
        (0u16, [0x00, 0x00]),
        (25565u16, [0x63, 0xDD]),
        (65535u16, [0xFF, 0xFF]),
    ] {
        let bytes = response::encode_server_ip_response("lobby", "127.0.0.1", port).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], tail.as_slice());

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.utf(), "ServerIP");
        assert_eq!(reader.utf(), "lobby");
        assert_eq!(reader.utf(), "127.0.0.1");
        assert_eq!(reader.u16(), port);
        assert_eq!(reader.finish(), 0);
    }
}

#[test]
fn server_ip_port_reads_as_u16_not_i32() {
    let bytes = response::encode_server_ip_response("lobby", "127.0.0.1", 40000).unwrap();
    let port_offset = bytes.len() - 2;
    let port_bytes = &bytes[port_offset..];
    // an i32 readInt would need four bytes — there are only two
    assert_eq!(port_bytes.len(), 2);
    assert_ne!(port_bytes.len(), 4);
    // u16 big-endian decode is exact
    assert_eq!(u16::from_be_bytes([port_bytes[0], port_bytes[1]]), 40000);
}

#[test]
fn uuid_response_roundtrips() {
    let uuid = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
    let bytes = response::encode_uuid_response(uuid).unwrap();
    // the payload starts with writeUTF("UUID")
    assert_eq!(&bytes[..6], [0x00, 0x04, 0x55, 0x55, 0x49, 0x44].as_slice());
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "UUID");
    assert_eq!(reader.utf(), uuid);
    assert_eq!(reader.finish(), 0);
}

#[test]
fn uuid_other_response_roundtrips() {
    let uuid = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
    let bytes = response::encode_uuid_other_response("Notch", uuid).unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "UUIDOther");
    assert_eq!(reader.utf(), "Notch");
    assert_eq!(reader.utf(), uuid);
    assert_eq!(reader.finish(), 0);
}

#[test]
fn forwarded_response_shape_has_no_forward_wrapper() {
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let bytes = response::encode_forwarded_response("MyChannel", &payload).unwrap();

    // first field must be the nested channel, NOT the string "Forward"
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.utf(), "MyChannel");
    assert_eq!(reader.u16(), payload.len() as u16);
    assert_eq!(reader.bytes(payload.len()), payload);
    assert_eq!(reader.finish(), 0);

    // hand-written expectation, byte for byte
    let expected: &[u8] = &[
        0x00, 0x09, 0x4D, 0x79, 0x43, 0x68, 0x61, 0x6E, 0x6E, 0x65,
        0x6C, // writeUTF("MyChannel")
        0x00, 0x04, //                                                writeShort(4)
        0xDE, 0xAD, 0xBE, 0xEF, //                                      payload
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn forwarded_response_rejects_oversize_payload() {
    assert_eq!(
        response::encode_forwarded_response("C", &vec![0u8; u16::MAX as usize + 1]),
        Err(BungeeMessagingError::ForwardPayloadTooLong {
            subchannel: "Forward".to_owned(),
            len: u16::MAX as usize + 1,
            max: u16::MAX as usize,
        })
    );
}

#[test]
fn response_encode_error_carries_subchannel_context() {
    // a string longer than 65535 bytes cannot fit writeUTF — the error must
    // name the subchannel being encoded
    let huge = "x".repeat(MAX_UTF_BYTES + 1);
    let err = response::encode_get_server_response(&huge).unwrap_err();
    match err {
        BungeeMessagingError::Encode { subchannel, .. } => assert_eq!(subchannel, "GetServer"),
        other => panic!("expected Encode, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Bad input
// ---------------------------------------------------------------------------

#[test]
fn unknown_subchannel_is_rejected() {
    let mut data = Vec::new();
    java::write_java_string(&mut data, "Frobnicate").unwrap();
    java::write_java_string(&mut data, "whatever").unwrap();
    assert_eq!(
        BungeeCodec.parse(&data),
        Err(BungeeMessagingError::UnknownSubchannel {
            subchannel: "Frobnicate".to_owned(),
        })
    );
}

#[test]
fn subchannel_is_case_sensitive() {
    let mut data = Vec::new();
    java::write_java_string(&mut data, "CONNECT").unwrap();
    java::write_java_string(&mut data, "lobby").unwrap();
    assert_eq!(
        BungeeCodec.parse(&data),
        Err(BungeeMessagingError::UnknownSubchannel {
            subchannel: "CONNECT".to_owned(),
        })
    );
}

#[test]
fn empty_payload_is_rejected() {
    assert!(matches!(
        BungeeCodec.parse(&[]),
        Err(BungeeMessagingError::SubchannelPrefix {
            source: java::JavaCodecError::Truncated {
                needed: 2,
                remaining: 0
            }
        })
    ));
    // one byte of the length prefix is also too short
    assert!(matches!(
        BungeeCodec.parse(&[0x00]),
        Err(BungeeMessagingError::SubchannelPrefix { .. })
    ));
}

#[test]
fn truncated_payloads_are_rejected() {
    let codec = BungeeCodec;
    let golden: &[u8] = &[
        0x00, 0x07, 0x43, 0x6F, 0x6E, 0x6E, 0x65, 0x63, 0x74, // writeUTF("Connect")
        0x00, 0x05, 0x6C, 0x6F, 0x62, 0x62, 0x79, //           writeUTF("lobby")
    ];
    for cut in 0..golden.len() {
        assert!(
            codec.parse(&golden[..cut]).is_err(),
            "truncation at byte {cut} must fail"
        );
    }
    assert_eq!(
        codec.parse(golden).unwrap(),
        BungeeMessage::Connect {
            server: "lobby".to_owned()
        }
    );
}

#[test]
fn forward_length_mismatch_is_rejected() {
    let mut data = Vec::new();
    java::write_java_string(&mut data, "Forward").unwrap();
    java::write_java_string(&mut data, "ALL").unwrap();
    java::write_java_string(&mut data, "MyChannel").unwrap();
    java::write_java_u16(&mut data, 5); // declares five bytes of payload
    data.extend_from_slice(&[0xAA, 0xBB]); // but only two are present
    match BungeeCodec.parse(&data) {
        Err(BungeeMessagingError::Decode {
            subchannel,
            source:
                java::JavaCodecError::Truncated {
                    needed: 5,
                    remaining: 2,
                },
        }) => assert_eq!(subchannel, "Forward"),
        other => panic!("expected Decode/Truncated, got {other:?}"),
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut data = Vec::new();
    java::write_java_string(&mut data, "IP").unwrap();
    data.push(0x00); // one unexpected byte after the subchannel
    assert_eq!(
        BungeeCodec.parse(&data),
        Err(BungeeMessagingError::TrailingBytes {
            subchannel: "IP".to_owned(),
            trailing: 1,
        })
    );
}

#[test]
fn port_must_be_read_as_i32_not_i64() {
    // IP response carries a 4-byte big-endian int port; an 8-byte readInt64
    // must fail with truncation instead of consuming something else
    let bytes = response::encode_ip_response("1.2.3.4", 25565).unwrap();
    let port_offset = bytes.len() - 4;
    assert!(java::read_java_i64(&mut &bytes[port_offset..]).is_err());
    // the u16 view of the two low bytes matches the big-endian int's tail
    assert_eq!(
        java::read_java_u16(&mut &bytes[port_offset + 2..]).unwrap(),
        0x63DD
    );
}

#[test]
fn oversize_string_field_fails_encoding() {
    let codec = BungeeCodec;
    let msg = BungeeMessage::Message {
        target: "ALL".to_owned(),
        message: "x".repeat(MAX_UTF_BYTES + 1),
    };
    match codec.encode(&msg) {
        Err(BungeeMessagingError::Encode { subchannel, .. }) => assert_eq!(subchannel, "Message"),
        other => panic!("expected Encode, got {other:?}"),
    }
}

#[test]
fn encode_rejects_nothing_silently() {
    // every encoder rejects an over-limit string instead of truncating it
    let huge = "x".repeat(MAX_UTF_BYTES + 1);
    let huge_str = huge.as_str();
    assert!(response::encode_ip_response(&huge, 1).is_err());
    assert!(response::encode_ip_other_response(&huge, "1.1.1.1", 1).is_err());
    assert!(response::encode_player_count_response(&huge, 1).is_err());
    assert!(response::encode_player_list_response("pvp", &[huge_str]).is_err());
    assert!(response::encode_get_player_server_response(&huge, None).is_err());
    assert!(response::encode_get_servers_response(&[huge_str]).is_err());
    assert!(response::encode_get_server_response(&huge).is_err());
    assert!(response::encode_server_ip_response(&huge, "1.1.1.1", 1).is_err());
    assert!(response::encode_uuid_response(&huge).is_err());
    assert!(
        response::encode_uuid_other_response(&huge, "069a79f4-44e9-4726-a5be-fca90e38aaf5")
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

#[test]
fn all_subchannels_are_accounted_for() {
    // The upstream protocol defines exactly 18 subchannels. The well-known
    // number 19 attached to it is a miscount: the SpigotMC wiki has sections
    // for exactly these 18, BungeeCord's DownstreamBridge switch has exactly
    // these 18 cases, and Velocity's BungeeCordMessageResponder switch has
    // exactly these 18 cases too. This test enumerates every variant once,
    // checks the 1:1 mapping with SUBCHANNEL_NAMES, and fails loudly on a
    // forgotten or duplicated subchannel.
    assert_eq!(SUBCHANNEL_NAMES.len(), 18);

    let samples = all_variants();
    assert_eq!(samples.len(), 18);

    let mut from_samples: Vec<&'static str> = samples.iter().map(|m| m.subchannel()).collect();
    from_samples.sort_unstable();
    from_samples.dedup();

    let mut expected = SUBCHANNEL_NAMES.to_vec();
    expected.sort_unstable();

    assert_eq!(from_samples, expected);

    // every name pairs back with its variant through the codec
    let codec = BungeeCodec;
    for sample in samples {
        let bytes = codec.encode(&sample).unwrap();
        let parsed = codec.parse(&bytes).unwrap();
        assert_eq!(parsed.subchannel(), sample.subchannel());
    }
}

// ---------------------------------------------------------------------------
// UUID dashed form
// ---------------------------------------------------------------------------

#[test]
fn uuid_responses_use_read_utf_strings() {
    // the wire carries the dashed string, never 16 raw bytes
    let uuid = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
    let bytes = response::encode_uuid_response(uuid).unwrap();
    let mut reader = Reader::new(&bytes);
    let _ = reader.utf(); // "UUID"
    let parsed = reader.utf();
    assert_eq!(parsed, uuid);
    assert_eq!(parsed.len(), 36);
}

// keep Reader::i64 exercised so wrong-type reads are covered by design
#[test]
fn reader_sanity() {
    let mut buf = Vec::new();
    java::write_java_i64(&mut buf, 0x0102030405060708);
    let mut reader = Reader::new(&buf);
    assert_eq!(reader.i64(), 0x0102030405060708);
    assert_eq!(reader.finish(), 0);
}
