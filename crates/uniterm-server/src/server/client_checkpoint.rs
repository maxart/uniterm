//! Serialize only the undecoded part of a client request.

use super::{ClientSession, FrameDecoder, MAX_CLIENT_FRAME, MAX_PENDING_CLIENT};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

impl ClientSession {
    pub(super) fn validate(&self) -> std::io::Result<()> {
        if self.dead
            || self.out_offset > self.outbuf.len()
            || self.outbuf.len() > MAX_PENDING_CLIENT
            || self
                .render_end
                .is_some_and(|end| end <= self.out_offset || end > self.outbuf.len())
            || self
                .final_frame
                .as_ref()
                .is_some_and(|frame| frame.len() > MAX_PENDING_CLIENT)
            || self.direct.is_some() && !self.direct_only
            || self.direct_only && self.attached
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid client checkpoint",
            ));
        }
        Ok(())
    }
}

pub(super) mod decoder {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        decoder: &FrameDecoder,
        out: S,
    ) -> Result<S::Ok, S::Error> {
        let bytes = decoder
            .pending_bytes()
            .map_err(|_| serde::ser::Error::custom("failed client decoder"))?;
        bytes.serialize(out)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<FrameDecoder, D::Error> {
        let bytes = Vec::<u8>::deserialize(input)?;
        if let Some(header) = bytes.first_chunk::<4>() {
            let size = u32::from_be_bytes(*header) as usize;
            if bytes.len() - 4 >= size {
                return Err(serde::de::Error::custom(
                    "complete request in client checkpoint",
                ));
            }
        }
        let mut decoder = FrameDecoder::with_max_frame(MAX_CLIENT_FRAME);
        decoder.push(&bytes);
        decoder
            .pending_bytes()
            .map_err(|_| serde::de::Error::custom("invalid pending client frame"))?;
        Ok(decoder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ClientMessage, DirectAttachment, PaneAttachRole, PaneId};

    #[test]
    fn restored_observer_retains_restrictions_and_pending_paste() {
        let mut session = ClientSession {
            direct_only: true,
            direct: Some(DirectAttachment {
                pane: PaneId(3),
                role: PaneAttachRole::Observer,
                last_cursor_visible: Some(false),
            }),
            ..Default::default()
        };
        for byte in b"\x1b[200" {
            session.keyboard_input.observe_byte(*byte);
        }
        let packet = bincode::serialize(&session).unwrap();
        let mut restored: ClientSession = bincode::deserialize(&packet).unwrap();
        restored.validate().unwrap();
        assert!(restored.direct_only);
        assert!(!restored.direct.as_ref().unwrap().role.can_control());
        assert!(restored.keyboard_input.observe_byte(b'~'));
        assert!(restored.keyboard_input.observe_byte(2));
        restored.attached = true;
        assert!(restored.validate().is_err());
        restored.attached = false;
        restored.direct_only = false;
        assert!(restored.validate().is_err());
    }

    #[test]
    fn client_checkpoints_only_resume_partial_requests_and_valid_output_offsets() {
        let mut session = ClientSession::default();
        let request = uniterm_proto::encode_frame(&ClientMessage::ListInfo);
        for byte in request {
            let packet = bincode::serialize(&session).unwrap();
            let restored: ClientSession = bincode::deserialize(&packet).unwrap();
            assert_eq!(
                restored.decoder.pending_bytes(),
                session.decoder.pending_bytes()
            );
            session.decoder.push(&[byte]);
        }
        assert!(
            bincode::deserialize::<ClientSession>(&bincode::serialize(&session).unwrap()).is_err()
        );
        session.outbuf = vec![1, 2, 3];
        session.out_offset = 2;
        session.render_end = Some(2);
        assert!(session.validate().is_err());
        session.render_end = Some(3);
        session.validate().unwrap();
    }
}
