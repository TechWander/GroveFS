//! Wire-protocol codec round-trip tests. No sockets, pure buffer math.

use yourfs::remote::proto;

#[test]
fn key_and_int_roundtrip() {
    let mut buf = Vec::new();
    proto::put_key(&mut buf, "hello-世界");
    proto::put_u32(&mut buf, 0xDEAD_BEEF);
    proto::put_u64(&mut buf, u64::MAX);

    let mut i = 0usize;
    assert_eq!(proto::get_key(&buf, &mut i).unwrap(), "hello-世界");
    assert_eq!(proto::read_u32(&buf, &mut i).unwrap(), 0xDEAD_BEEF);
    assert_eq!(proto::read_u64(&buf, &mut i).unwrap(), u64::MAX);
    assert_eq!(i, buf.len());
}

#[test]
fn short_frame_is_error_not_panic() {
    // One byte is not enough to hold a u16 length prefix.
    let body = [0u8];
    let mut i = 0usize;
    assert!(proto::read_u16(&body, &mut i).is_err());
    // A declared key longer than the buffer must be rejected.
    let mut buf = Vec::new();
    proto::put_key(&mut buf, "abc");
    // Corrupt the length prefix to claim more bytes than exist.
    buf[0..2].copy_from_slice(&(9999u16).to_be_bytes());
    let mut i = 0usize;
    assert!(proto::get_key(&buf, &mut i).is_err());
}

#[test]
fn request_frame_roundtrip() {
    let mut stream_buf = Vec::new();
    proto::send_request(&mut stream_buf, proto::OP_WRITE, b"payload-bytes").unwrap();

    let mut r = &stream_buf[..];
    let (op, body) = proto::recv_request(&mut r).unwrap();
    assert_eq!(op, proto::OP_WRITE);
    assert_eq!(body, b"payload-bytes");
}

#[test]
fn response_frame_roundtrip() {
    let mut stream_buf = Vec::new();
    proto::send_response(&mut stream_buf, -5, 0x1234_ABCD, b"data!").unwrap();

    let mut r = &stream_buf[..];
    let (status, val, data) = proto::recv_response(&mut r).unwrap();
    assert_eq!(status, -5);
    assert_eq!(val, 0x1234_ABCD);
    assert_eq!(data, b"data!");
}

#[test]
fn empty_body_and_data_frames() {
    let mut buf = Vec::new();
    proto::send_request(&mut buf, proto::OP_STATFS, &[]).unwrap();
    let mut r = &buf[..];
    let (op, body) = proto::recv_request(&mut r).unwrap();
    assert_eq!(op, proto::OP_STATFS);
    assert!(body.is_empty());

    let mut buf = Vec::new();
    proto::send_response(&mut buf, 0, 42, &[]).unwrap();
    let mut r = &buf[..];
    let (status, val, data) = proto::recv_response(&mut r).unwrap();
    assert_eq!((status, val), (0, 42));
    assert!(data.is_empty());
}

#[test]
fn bad_magic_is_rejected() {
    let mut buf = Vec::new();
    proto::send_request(&mut buf, proto::OP_READ, b"x").unwrap();
    // Corrupt the magic.
    buf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let mut r = &buf[..];
    assert!(proto::recv_request(&mut r).is_err());
}
