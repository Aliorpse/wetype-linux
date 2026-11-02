//! Framing helpers on top of [`LengthDelimitedCodec`].
//!
//! One frame = 4-byte big-endian length, then one encoded protobuf message.
//! Using the codec instead of hand-rolled `read_exact`/`write_all` keeps the
//! async paths at zero framing code: `Framed`/`FramedRead`/`FramedWrite` do the
//! work, and the helpers here cover the non-async bits (encoding into a buffer,
//! decoding one frame from a buffer).

use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use tokio_util::codec::{Decoder, LengthDelimitedCodec};

/// Upper bound on a single frame. A candidate batch is a few KiB; anything
/// approaching this is a protocol error or a corrupted length prefix, and is
/// rejected instead of being buffered forever.
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

/// The codec both hops use. The default `LengthDelimitedCodec` shape (4-byte
/// big-endian length) is exactly what we want, so this is only about the limits.
pub fn ipc_codec() -> LengthDelimitedCodec {
    let mut codec = LengthDelimitedCodec::new();
    codec.set_max_frame_length(MAX_FRAME_LEN);
    codec
}

/// Encode `msg` as one frame and append it to `dst`.
///
/// Encodes into a scratch buffer first so a serialization failure cannot leave a
/// half-written length prefix on the wire.
pub fn encode_frame<M: Message>(msg: &M, dst: &mut BytesMut) -> Result<(), prost::EncodeError> {
    let mut scratch = BytesMut::with_capacity(msg.encoded_len() + 4);
    scratch.put_u32(msg.encoded_len() as u32);
    msg.encode(&mut scratch)?;
    dst.unsplit(scratch);
    Ok(())
}

/// Decode at most one frame from `src`.
///
/// `Ok(None)` means "need more bytes". `Err` covers framing violations (frame
/// too large, negative length) and surfaces them early rather than de-synchronizing
/// the stream.
///
/// The returned [`Bytes`] is a frozen view of the codec's buffer, so parsing it
/// with `prost::Message::decode` does not copy the payload.
pub fn decode_frame(
    src: &mut BytesMut,
    codec: &mut LengthDelimitedCodec,
) -> std::io::Result<Option<Bytes>> {
    codec.decode(src).map(|frame| frame.map(BytesMut::freeze))
}
