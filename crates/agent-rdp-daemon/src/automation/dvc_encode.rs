//! Wraps a raw byte buffer as an `ironrdp_dvc::DvcMessage` so it goes through
//! the library's own DVC fragmentation instead of a hand-rolled single PDU.
//!
//! `rdp_session.rs` used to build one `DrdynvcDataPdu::Data` per outbound
//! automation message regardless of size. `DrdynvcDataPdu::MAX_DATA_SIZE` is
//! 1590 bytes; anything larger still got shipped as a single, oversized PDU,
//! which the static virtual channel layer beneath DVC (`CHANNEL_CHUNK_LENGTH
//! = 1600`) then split into multiple wire fragments anyway - but the
//! PowerShell agent's `Read-DvcMessage` treats every fragment as a complete
//! message and never reassembles, so every request whose JSON (including the
//! base64-encoded chunk payload for `file push`) crossed roughly 1.6KB failed
//! to parse on the agent side. `ironrdp_dvc::encode_dvc_messages` performs the
//! same splitting `DrdynvcClient` uses for its own channel data, correctly
//! emitting `DataFirst` + `Data` PDUs with the length/flags the agent's fixed
//! `CHANNEL_PDU_HEADER` parsing (once it reassembles, see `dvc.ps1`) expects.

use ironrdp_dvc::ironrdp_pdu::{Encode, EncodeResult, WriteCursor};
use ironrdp_dvc::{DvcEncode, DvcMessage};

/// A DVC message whose "encoding" is just the raw bytes handed in - lets
/// arbitrary already-serialized JSON go through `encode_dvc_messages`'s
/// splitting logic without needing a dedicated PDU type.
pub struct RawDvcBytes(pub Vec<u8>);

impl Encode for RawDvcBytes {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RawDvcBytes"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for RawDvcBytes {}

/// Encode `data` for `channel_id`, splitting into `DataFirst`/`Data` PDUs
/// per `ironrdp_dvc::DrdynvcDataPdu::MAX_DATA_SIZE` (1590 bytes) when needed.
pub fn encode_dvc_data(channel_id: u32, data: Vec<u8>) -> EncodeResult<Vec<ironrdp_svc::SvcMessage>> {
    let message: DvcMessage = Box::new(RawDvcBytes(data));
    ironrdp_dvc::encode_dvc_messages(channel_id, vec![message], ironrdp_svc::ChannelFlags::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_dvc::pdu::DrdynvcDataPdu;

    #[test]
    fn small_payload_is_a_single_pdu() {
        let data = vec![b'x'; 100];
        let messages = encode_dvc_data(7, data).expect("encode small payload");
        assert_eq!(messages.len(), 1, "a 100-byte payload must fit in one DVC PDU");
    }

    #[test]
    fn payload_over_max_data_size_is_split() {
        // 192KB, matching handlers::file_transfer::CHUNK_BYTES's raw chunk
        // size once base64-expanded (~256KB) - the exact case that used to
        // reach the agent as unparseable fragments.
        let data = vec![b'y'; 256 * 1024];
        let messages = encode_dvc_data(7, data).expect("encode large payload");
        assert!(
            messages.len() > 1,
            "a 256KB payload must be split into multiple DVC PDUs (MAX_DATA_SIZE = {})",
            DrdynvcDataPdu::MAX_DATA_SIZE
        );
    }

    #[test]
    fn boundary_payload_stays_single_pdu() {
        // Exactly MAX_DATA_SIZE must not split (the library's `>=` check
        // means MAX_DATA_SIZE itself already needs a DataFirst - confirm the
        // boundary matches what MAX_DATA_SIZE - 1 does not).
        let data = vec![b'z'; DrdynvcDataPdu::MAX_DATA_SIZE - 1];
        let messages = encode_dvc_data(7, data).expect("encode boundary payload");
        assert_eq!(messages.len(), 1);
    }
}
