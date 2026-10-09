//! Port of the pty project's `tests/protocol.test.ts`.

use pty_core::protocol::{
    AcceptedSocketOwnershipRequest, LifecycleCompareAndSetRequest, MAX_PACKET_LENGTH, MessageType,
    PacketReader, TcpConnectionTuple, decode_accepted_socket_ownership_request,
    decode_attach_identity, decode_cell, decode_exit, decode_geometry,
    decode_lifecycle_compare_and_set_request, decode_size, encode_accepted_socket_ownership_request,
    encode_attach, encode_attach_with_cell, encode_attach_with_identity, encode_data, encode_detach,
    encode_exit, encode_geometry, encode_lifecycle_compare_and_set_request, encode_packet,
    encode_resize, encode_resize_with_cell, encode_screen, encode_status, encode_status_response,
};
use pty_core::stats::{ClientStats, ConnectionStats, Constrains, StatsResult};

// ── encodePacket / PacketReader round-trips ──

#[test]
fn round_trips_data() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_data(b"hello world")).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Data);
    assert_eq!(packets[0].payload, b"hello world");
}

#[test]
fn connection_error_is_a_typed_non_exit_frame() {
    use pty_core::protocol::{ConnectionErrorReason, decode_connection_error, encode_connection_error};
    let frame = encode_connection_error(ConnectionErrorReason::ClientTooSlow);
    assert_eq!(frame, [12, 0, 0, 0, 1, 0]);
    let packet = PacketReader::new().feed(&frame).unwrap().remove(0);
    assert_eq!(packet.type_, MessageType::ConnectionError);
    assert_eq!(decode_connection_error(&packet.payload), Some(ConnectionErrorReason::ClientTooSlow));
    assert_eq!(decode_connection_error(&[255]), None);
    assert_eq!(decode_connection_error(&[]), None);
}

#[test]
fn round_trips_attach() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_attach(24, 80)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Attach);
    assert_eq!(decode_size(&packets[0].payload), (24, 80));
}

/// node: tests/protocol.test.ts:49-59
#[test]
fn attach_byte_identical_to_hand_built_packet() {
    assert_eq!(
        encode_attach(24, 80),
        encode_packet(MessageType::Attach, &[0, 24, 0, 80])
    );
}

/// The cell pixel size is an optional suffix on a size payload: a reader that
/// does not know about it takes the same rows and cols it always did, which
/// is what makes this safe to send to any daemon.
#[test]
fn attach_can_declare_a_cell_size_without_changing_the_size_it_carries() {
    let with_cell = encode_attach_with_cell(24, 80, 9, 18);
    assert_eq!(
        with_cell,
        encode_packet(MessageType::Attach, &[0, 24, 0, 80, 0, 9, 0, 18])
    );

    let mut reader = PacketReader::new();
    let packets = reader.feed(&with_cell).unwrap();
    assert_eq!(packets[0].type_, MessageType::Attach);
    assert_eq!(
        decode_size(&packets[0].payload),
        (24, 80),
        "the size is where it always was"
    );
    assert_eq!(decode_cell(&packets[0].payload), Some((9, 18)));
}

#[test]
fn attach_identity_preserves_legacy_size_and_accepts_missing_tty() {
    let mut reader = PacketReader::new();
    let packets = reader
        .feed(&encode_attach_with_identity(30, 90, 1234, Some("/dev/pts/7")))
        .unwrap();
    let payload = &packets[0].payload;
    assert_eq!(decode_size(payload), (30, 90));
    assert_eq!(decode_cell(payload), None);
    assert_eq!(decode_attach_identity(payload), (Some(1234), Some("/dev/pts/7".into())));
    assert_eq!(decode_attach_identity(&encode_attach(30, 90)[5..]), (None, None));
    let no_tty = reader.feed(&encode_attach_with_identity(30, 90, 5678, None)).unwrap();
    assert_eq!(decode_attach_identity(&no_tty[0].payload), (Some(5678), None));
}

#[test]
fn resize_can_declare_a_cell_size() {
    let mut reader = PacketReader::new();
    let packets = reader
        .feed(&encode_resize_with_cell(30, 100, 7, 15))
        .unwrap();
    assert_eq!(packets[0].type_, MessageType::Resize);
    assert_eq!(decode_size(&packets[0].payload), (30, 100));
    assert_eq!(decode_cell(&packets[0].payload), Some((7, 15)));
}

/// No declaration is the normal case, and it must not be mistaken for one.
#[test]
fn a_plain_size_payload_declares_no_cell() {
    assert_eq!(decode_cell(&encode_attach(24, 80)[5..]), None);
    assert_eq!(decode_cell(&encode_resize(24, 80)[5..]), None);
    assert_eq!(decode_cell(&[]), None);
    assert_eq!(
        decode_cell(&[0, 24, 0, 80, 0, 0, 0, 0]),
        None,
        "a zero cell is no cell"
    );
}

/// node: tests/protocol.test.ts:288-297
#[test]
fn round_trips_geometry() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_geometry(24, 80)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Geometry);
    assert_eq!(packets[0].type_.as_u8(), 10);
    assert_eq!(MessageType::from_u8(10), MessageType::Geometry);
    assert_eq!(decode_geometry(&packets[0].payload), (24, 80));
    assert_eq!(decode_geometry(&[0, 1]), (24, 80));
}

/// node: tests/protocol.test.ts:299-313
#[test]
fn older_client_skips_geometry_and_continues_with_data() {
    let mut reader = PacketReader::new();
    let mut raw = encode_geometry(24, 80);
    raw.extend_from_slice(&encode_data(b"after-unknown"));
    let mut received = Vec::new();
    for p in reader.feed(&raw).unwrap() {
        // Models the pre-GEOMETRY client switch, which only handles DATA.
        if p.type_ == MessageType::Data {
            received.extend_from_slice(&p.payload);
        }
    }
    assert_eq!(received, b"after-unknown");
}

#[test]
fn round_trips_detach() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_detach()).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Detach);
    assert_eq!(packets[0].payload.len(), 0);
}

#[test]
fn round_trips_resize() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_resize(48, 120)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(decode_size(&packets[0].payload), (48, 120));
}

#[test]
fn round_trips_exit() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_exit(42)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Exit);
    assert_eq!(decode_exit(&packets[0].payload), 42);
}

#[test]
fn round_trips_screen() {
    let mut reader = PacketReader::new();
    let screen = "\x1b[2J\x1b[H$ hello\r\nworld";
    let packets = reader.feed(&encode_screen(screen.as_bytes())).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Screen);
    assert_eq!(packets[0].payload, screen.as_bytes());
}

// ── streaming ──

#[test]
fn multiple_packets_in_one_chunk() {
    let mut reader = PacketReader::new();
    let mut buf = Vec::new();
    buf.extend_from_slice(&encode_data(b"hello"));
    buf.extend_from_slice(&encode_data(b"world"));
    buf.extend_from_slice(&encode_detach());
    let packets = reader.feed(&buf).unwrap();
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].payload, b"hello");
    assert_eq!(packets[1].payload, b"world");
    assert_eq!(packets[2].type_, MessageType::Detach);
}

#[test]
fn packets_split_across_chunks() {
    let mut reader = PacketReader::new();
    let full = encode_data(b"hello world");
    assert_eq!(reader.feed(&full[0..3]).unwrap().len(), 0);
    assert_eq!(reader.feed(&full[3..8]).unwrap().len(), 0);
    let packets = reader.feed(&full[8..]).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].payload, b"hello world");
}

#[test]
fn packet_split_at_header_boundary() {
    let mut reader = PacketReader::new();
    let full = encode_data(b"test");
    assert_eq!(reader.feed(&full[0..5]).unwrap().len(), 0);
    let packets = reader.feed(&full[5..]).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].payload, b"test");
}

#[test]
fn empty_payload() {
    let mut reader = PacketReader::new();
    let packets = reader
        .feed(&encode_packet(MessageType::Detach, &[]))
        .unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Detach);
    assert_eq!(packets[0].payload.len(), 0);
}

#[test]
fn large_payloads() {
    let mut reader = PacketReader::new();
    let big = vec![b'x'; 100_000];
    let packets = reader.feed(&encode_data(&big)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].payload, big);
}

#[test]
fn ignores_unknown_message_types() {
    let mut reader = PacketReader::new();
    let mut raw = vec![99u8];
    raw.extend_from_slice(&3u32.to_be_bytes());
    raw.extend_from_slice(b"abc");
    let packets = reader.feed(&raw).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Unknown(99));
    assert_eq!(packets[0].payload, b"abc");
}

// ── decode edge cases ──

#[test]
fn decode_size_defaults() {
    assert_eq!(decode_size(&[0, 0]), (24, 80));
    assert_eq!(decode_size(&[]), (24, 80));
}

#[test]
fn decode_exit_defaults() {
    assert_eq!(decode_exit(&[0, 0]), -1);
    assert_eq!(decode_exit(&[]), -1);
}

// ── STATUS + oversize ──

#[test]
fn round_trips_status_request() {
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_status()).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Status);
    assert_eq!(packets[0].payload.len(), 0);
}

#[test]
fn rejects_oversize_length() {
    let mut reader = PacketReader::new();
    let mut header = vec![MessageType::Data.as_u8()];
    header.extend_from_slice(&((MAX_PACKET_LENGTH as u32) + 1).to_be_bytes());
    let err = reader.feed(&header).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Packet length {} exceeds maximum {}",
            MAX_PACKET_LENGTH + 1,
            MAX_PACKET_LENGTH
        )
    );
}

#[test]
fn rejects_max_uint32_length() {
    let mut reader = PacketReader::new();
    let mut header = vec![MessageType::Data.as_u8()];
    header.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
    assert!(reader.feed(&header).is_err());
}

#[test]
fn poisons_buffer_after_oversize() {
    let mut reader = PacketReader::new();
    let mut header = vec![MessageType::Data.as_u8()];
    header.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
    let _ = reader.feed(&header);
    let packets = reader.feed(&encode_data(b"hi")).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].payload, b"hi");
}

#[test]
fn round_trips_status_response() {
    let mut reader = PacketReader::new();
    let json = r#"{"name":"test","terminal":{"cols":80,"rows":24}}"#;
    let packets = reader.feed(&encode_status_response(json)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Status);
    assert_eq!(String::from_utf8_lossy(&packets[0].payload), json);
}

/// node: tests/protocol.test.ts:262-286
#[test]
fn round_trips_status_response_with_connections() {
    let mut reader = PacketReader::new();
    let json = r#"{"name":"test","terminal":{"cols":80,"rows":24},"clients":{"total":1,"attached":1,"readOnly":0,"connections":[{"role":"writable","rows":24,"cols":80,"lastRequestSequence":1,"constrains":{"rows":true,"cols":true}}]}}"#;
    let packets = reader.feed(&encode_status_response(json)).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].type_, MessageType::Status);
    let body: serde_json::Value = serde_json::from_slice(&packets[0].payload).unwrap();
    assert_eq!(
        body,
        serde_json::from_str::<serde_json::Value>(json).unwrap()
    );

    // The typed `clients` shape serializes to exactly the Node key order.
    let clients: ClientStats = serde_json::from_value(body["clients"].clone()).unwrap();
    assert_eq!(
        clients.connections,
        Some(vec![ConnectionStats::Writable {
            rows: 24,
            cols: 80,
            last_request_sequence: 1,
            constrains: Constrains {
                rows: true,
                cols: true
            },
        }])
    );
    assert_eq!(
        serde_json::to_string(&clients).unwrap(),
        r#"{"total":1,"attached":1,"readOnly":0,"connections":[{"role":"writable","rows":24,"cols":80,"lastRequestSequence":1,"constrains":{"rows":true,"cols":true}}]}"#
    );
    let readonly = ConnectionStats::Readonly {
        constrains: Constrains {
            rows: false,
            cols: false,
        },
    };
    assert_eq!(
        serde_json::to_string(&readonly).unwrap(),
        r#"{"role":"readonly","constrains":{"rows":false,"cols":false}}"#
    );
}

/// node: tests/protocol.test.ts:315-345
#[test]
fn accepts_legacy_status_without_connection_details() {
    let json = r#"{"name":"legacy","terminal":{"cols":80,"rows":24,"cursorX":0,"cursorY":0,"scrollbackUsed":24,"scrollbackCapacity":10024},"process":{"alive":true,"exitCode":null,"pid":123,"resources":null},"daemon":{"pid":456,"resources":null},"clients":{"total":2,"attached":2,"readOnly":0},"modes":{"sgrMouse":false,"cursorHidden":false,"kittyKeyboard":false,"kittyKeyboardFlags":[]},"uptimeSeconds":10,"createdAt":"2026-07-31T00:00:00.000Z"}"#;
    let mut reader = PacketReader::new();
    let packets = reader.feed(&encode_status_response(json)).unwrap();
    let decoded: StatsResult = serde_json::from_slice(&packets[0].payload).unwrap();
    assert_eq!(decoded.clients.total, 2);
    assert_eq!(decoded.clients.attached, 2);
    assert_eq!(decoded.clients.read_only, 0);
    assert!(decoded.clients.connections.is_none());
    // Re-serializing keeps `connections` absent, as Node's JSON would.
    assert_eq!(
        serde_json::to_string(&decoded.clients).unwrap(),
        r#"{"total":2,"attached":2,"readOnly":0}"#
    );
    assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
}

#[test]
fn readiness_requests_use_node_wire_tags_and_json() {
    let ownership = AcceptedSocketOwnershipRequest {
        expected_generation: "generation-a".to_string(),
        connection: TcpConnectionTuple {
            local_address: "127.0.0.1".to_string(),
            local_port: 51_000,
            remote_address: "127.0.0.1".to_string(),
            remote_port: 3_000,
        },
    };
    let bytes = encode_accepted_socket_ownership_request(&ownership);
    assert_eq!(bytes[0], 8);
    let mut reader = PacketReader::new();
    let packet = reader.feed(&bytes).unwrap().remove(0);
    assert_eq!(
        decode_accepted_socket_ownership_request(&packet.payload),
        Some(ownership)
    );
    assert_eq!(
        std::str::from_utf8(&packet.payload).unwrap(),
        r#"{"expectedGeneration":"generation-a","connection":{"localAddress":"127.0.0.1","localPort":51000,"remoteAddress":"127.0.0.1","remotePort":3000}}"#
    );

    let cas = LifecycleCompareAndSetRequest {
        expected_generation: "generation-a".to_string(),
        tag: "run.lifecycle".to_string(),
        expected_value: "starting".to_string(),
        value: "ready".to_string(),
    };
    let bytes = encode_lifecycle_compare_and_set_request(&cas);
    assert_eq!(bytes[0], 9);
    let packet = PacketReader::new().feed(&bytes).unwrap().remove(0);
    assert_eq!(
        decode_lifecycle_compare_and_set_request(&packet.payload),
        Some(cas)
    );
}
