//! Wire-protocol codec round-trip tests. No sockets, pure buffer math.

use grovefs::remote::proto;

/// A writer that accepts at most ONE slice per call — exactly what the trait's
/// default `write_vectored` does. It proves the frame encoder loops until every
/// byte is out: a truncated frame would desync the connection for all later
/// requests on that socket.
struct OneSlicePerCall {
    out: Vec<u8>,
}

impl std::io::Write for OneSlicePerCall {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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

/// The parts encoder must put EXACTLY the same bytes on the wire as the
/// contiguous encoder: the frame format is the compatibility contract between
/// old and new client/server builds, so the zero-copy path cannot change it.
#[test]
fn parts_request_frame_matches_contiguous_frame() {
    let head = b"12uuid-key";
    let offset = 0x0102_0304_0506_0708u64.to_be_bytes();
    let payload = vec![0xABu8; 4096];

    let mut contiguous = Vec::new();
    let mut body = Vec::new();
    body.extend_from_slice(head);
    body.extend_from_slice(&offset);
    body.extend_from_slice(&payload);
    proto::send_request(&mut contiguous, proto::OP_WRITE, &body).unwrap();

    let mut parts = Vec::new();
    // The empty middle slice is deliberate: zero-length parts must be skipped,
    // not encoded as a gap in the body.
    proto::send_request_parts(&mut parts, proto::OP_WRITE, &[head, &[], &offset, &payload])
        .unwrap();

    assert_eq!(parts, contiguous, "parts frame differs from contiguous frame");
    let mut r = &parts[..];
    let (op, got) = proto::recv_request(&mut r).unwrap();
    assert_eq!(op, proto::OP_WRITE);
    assert_eq!(got, body);
}

#[test]
fn parts_response_frame_matches_contiguous_frame() {
    let a = b"chunk-a";
    let b = vec![0x5Au8; 1024];

    let mut contiguous = Vec::new();
    let mut data = Vec::new();
    data.extend_from_slice(a);
    data.extend_from_slice(&b);
    proto::send_response(&mut contiguous, 0, 77, &data).unwrap();

    let mut parts = Vec::new();
    proto::send_response_parts(&mut parts, 0, 77, &[a, &[], &b]).unwrap();
    assert_eq!(parts, contiguous, "parts response differs from contiguous response");

    let mut r = &parts[..];
    let (status, val, got) = proto::recv_response(&mut r).unwrap();
    assert_eq!((status, val), (0, 77));
    assert_eq!(got, data);
}

/// A writer that only ever consumes one iovec per call must still receive the
/// WHOLE frame (header + every part), or the peer would parse a short body.
#[test]
fn vectored_frame_completes_under_single_slice_writers() {
    let payload = vec![0x11u8; 8192];
    let mut w = OneSlicePerCall { out: Vec::new() };
    proto::send_request_parts(&mut w, proto::OP_WRITE, &[b"key", &payload]).unwrap();
    let mut r = &w.out[..];
    let (op, body) = proto::recv_request(&mut r).unwrap();
    assert_eq!(op, proto::OP_WRITE);
    assert_eq!(body.len(), 3 + payload.len());
    assert_eq!(&body[3..], &payload[..]);

    let mut w = OneSlicePerCall { out: Vec::new() };
    proto::send_response_parts(&mut w, 0, 1, &[b"head", &payload]).unwrap();
    let mut r = &w.out[..];
    let (status, val, data) = proto::recv_response(&mut r).unwrap();
    assert_eq!((status, val), (0, 1));
    assert_eq!(data.len(), 4 + payload.len());
}
