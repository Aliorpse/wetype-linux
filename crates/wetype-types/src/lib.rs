//! Shared wire types for the WeType-on-Linux stack.
//!
//! Three processes speak this protocol:
//!
//! ```text
//! fcitx5 addon  --unix socket-->  wetype-daemon  --stdio pipe-->  wetype-engine.exe
//! ```
//!
//! Both hops use the same messages and the same framing: one protobuf message
//! per frame, where a frame is a 4-byte big-endian length prefix followed by the
//! encoded message ([`codec::LengthDelimitedCodec`] default shape).
//!
//! Two rules shape the messages:
//!
//! * **No engine-private bytes cross the wire.** Selection is by index into a
//!   snapshot the engine host holds, so candidate ids and iterators stay inside
//!   the process that owns them.
//! * **The host decides what to commit.** [`ipc::SelectResult::commit`] is the
//!   authoritative text for the application; clients must not accumulate text
//!   themselves, which is what makes partial selection expressible.

/// Generated protobuf messages for `proto/wetype/ipc/ipc.proto`.
pub mod ipc {
    include!(concat!(env!("OUT_DIR"), "/wetype.ipc.rs"));
}

pub mod codec;

pub use codec::{MAX_FRAME_LEN, decode_frame, encode_frame, ipc_codec};

/// Bumped whenever the wire format changes incompatibly.
///
/// Exchanged in [`ipc::Ping`] so a mismatched pair fails loudly instead of
/// misparsing.
///
/// 2: `SelectResult` gained `rest` for partial selection. The old fields are
///    reserved, so an old daemon talking to a new addon still degrades to
///    whole-word selection instead of misparsing.
pub const PROTOCOL_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn sample_request() -> ipc::Request {
        ipc::Request {
            seq: 42,
            kind: Some(ipc::request::Kind::Select(ipc::Select {
                session: 7,
                generation: 3,
                index: 1,
            })),
        }
    }

    /// Encode, then decode exactly one frame, asserting the header length agrees
    /// with the payload and nothing is left over.
    fn roundtrip<M>(msg: &M) -> M
    where
        M: Message + Default + PartialEq + std::fmt::Debug,
    {
        let mut codec = ipc_codec();
        let mut buf = bytes::BytesMut::new();
        encode_frame(msg, &mut buf).expect("encode");
        assert!(!buf.is_empty());

        let declared = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(declared, buf.len() - 4, "frame header must match payload");

        let frame = decode_frame(&mut buf, &mut codec)
            .expect("decode")
            .expect("a whole frame");
        let back = M::decode(frame).expect("parse");
        assert!(buf.is_empty(), "no trailing bytes");
        back
    }

    #[test]
    fn request_roundtrips() {
        let back = roundtrip(&sample_request());
        assert_eq!(back, sample_request());
    }

    #[test]
    fn partial_frame_is_none_not_error() {
        let mut codec = ipc_codec();
        let mut whole = bytes::BytesMut::new();
        encode_frame(&sample_request(), &mut whole).expect("encode");

        // Everything except the last byte: the codec must ask for more, not fail.
        let mut cut = whole.split_to(whole.len() - 1);
        assert!(
            decode_frame(&mut cut, &mut codec)
                .expect("no error yet")
                .is_none()
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut codec = ipc_codec();
        let mut buf = bytes::BytesMut::new();
        // Claim a frame far beyond MAX_FRAME_LEN without sending the payload.
        buf.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(
            decode_frame(&mut buf, &mut codec).is_err(),
            "a 4 GiB frame must not be accepted"
        );
    }

    #[test]
    fn partial_selection_shape_roundtrips() {
        // "ni" + "hao": committing only "你" leaves "hao" composing.
        let rest = ipc::Candidates {
            session: 1,
            generation: 3,
            items: vec![ipc::Candidate {
                text: "好".as_bytes().to_vec(),
                cover: 3,
                cloud: false,
                comment: Vec::new(),
            }],
            truncated: false,
            preedit: b"hao".to_vec(),
            raw: b"hao".to_vec(),
            cursor: 3,
            commit: Vec::new(),
        };
        let result = ipc::SelectResult {
            commit: "你".as_bytes().to_vec(),
            finished: false,
            rest: Some(rest),
        };
        let back = roundtrip(&result);
        assert_eq!(back, result);
        assert!(
            !back.finished,
            "a partial selection must not end the composition"
        );
        assert_eq!(
            back.rest.as_ref().expect("rest must survive").raw,
            b"hao",
            "the rest of the pinyin is still composing"
        );
    }

    #[test]
    fn full_selection_clears_the_composition() {
        let result = ipc::SelectResult {
            commit: "你好".as_bytes().to_vec(),
            finished: true,
            rest: None,
        };
        let back = roundtrip(&result);
        assert_eq!(back, result);
        assert!(back.finished && back.rest.is_none());
    }

    #[test]
    fn comments_with_control_bytes_survive() {
        // Comments are text, but nothing stops a future engine version from
        // including control bytes; length framing (unlike newline framing)
        // carries them unchanged. Also covers the candidate list itself.
        let nasty = b"ok\x00\x0a\x0d\xff".to_vec();
        let cands = ipc::Candidates {
            session: 1,
            generation: 9,
            items: vec![ipc::Candidate {
                text: "你".as_bytes().to_vec(),
                cover: 1,
                cloud: false,
                comment: nasty.clone(),
            }],
            truncated: true,
            preedit: b"ni".to_vec(),
            cursor: 2,
            commit: b",".to_vec(),
            raw: b"ni".to_vec(),
        };
        let back = roundtrip(&cands);
        assert_eq!(back.items[0].comment, nasty);
        assert_eq!(back.generation, 9, "generation must survive for Select");
        assert_eq!(
            back.commit, b",",
            "an engine-emitted commit survives the wire"
        );
    }
}
