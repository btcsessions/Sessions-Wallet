//! Ledger's framing of APDUs over USB HID and Bluetooth LE.
//!
//! Both split an APDU into frames: `tag (0x05) | sequence (u16 BE) |
//! payload`, with the APDU's length (u16 BE) at the start of the first
//! frame's payload. USB HID frames are 64-byte reports that also start with
//! a channel (`0x0101`) and are zero-padded; Bluetooth frames are at most
//! the MTU the device reports and not padded.

/// Tag of APDU frames.
pub const TAG_APDU: u8 = 0x05;
/// Tag of the Bluetooth "what MTU?" exchange.
pub const TAG_MTU: u8 = 0x08;
/// USB HID channel the Ledger apps use.
pub const CHANNEL: u16 = 0x0101;
/// USB HID report size.
pub const HID_REPORT: usize = 64;

/// Split `apdu` into frames. `channel`: USB (`Some`), Bluetooth (`None`).
/// `size`: frame size (64 for USB, the device's MTU for Bluetooth).
pub fn frames(apdu: &[u8], channel: Option<u16>, size: usize) -> Vec<Vec<u8>> {
    let mut data = Vec::with_capacity(apdu.len() + 2);
    data.extend_from_slice(&(apdu.len() as u16).to_be_bytes());
    data.extend_from_slice(apdu);
    let header = if channel.is_some() { 5 } else { 3 };
    let room = size.saturating_sub(header).max(1);
    let mut out = Vec::new();
    for (seq, chunk) in data.chunks(room).enumerate() {
        let mut f = Vec::with_capacity(size);
        if let Some(c) = channel {
            f.extend_from_slice(&c.to_be_bytes());
        }
        f.push(TAG_APDU);
        f.extend_from_slice(&(seq as u16).to_be_bytes());
        f.extend_from_slice(chunk);
        if channel.is_some() {
            f.resize(size, 0);
        }
        out.push(f);
    }
    out
}

/// Reassembles a response from frames, in order.
#[derive(Debug, Default)]
pub struct Reader {
    channel: Option<u16>,
    want: Option<usize>,
    next_seq: u16,
    data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Not an APDU frame for us (another channel or tag): skip it.
    Ignore,
    /// Frames out of order or malformed: the exchange is broken.
    Bad(&'static str),
}

impl Reader {
    pub fn new(channel: Option<u16>) -> Self {
        Reader {
            channel,
            ..Default::default()
        }
    }

    /// Add one frame. `Ok(Some(response))` once the whole response is in
    /// (data then the 2-byte status word).
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, FrameError> {
        let mut f = frame;
        if let Some(c) = self.channel {
            if f.len() < 2 || u16::from_be_bytes([f[0], f[1]]) != c {
                return Err(FrameError::Ignore);
            }
            f = &f[2..];
        }
        if f.len() < 3 || f[0] != TAG_APDU {
            return Err(FrameError::Ignore);
        }
        let seq = u16::from_be_bytes([f[1], f[2]]);
        if seq != self.next_seq {
            return Err(FrameError::Bad("frames out of order"));
        }
        let mut payload = &f[3..];
        if seq == 0 {
            if payload.len() < 2 {
                return Err(FrameError::Bad("short first frame"));
            }
            self.want = Some(u16::from_be_bytes([payload[0], payload[1]]) as usize);
            payload = &payload[2..];
        }
        self.next_seq = self.next_seq.wrapping_add(1);
        let want = self.want.unwrap_or(0);
        let take = (want - self.data.len()).min(payload.len());
        self.data.extend_from_slice(&payload[..take]);
        if self.data.len() >= want {
            if want < 2 {
                return Err(FrameError::Bad("response without a status word"));
            }
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        Ok(None)
    }
}

/// The status word at the end of a response, and the data before it.
pub fn split_status(response: &[u8]) -> Option<(u16, &[u8])> {
    if response.len() < 2 {
        return None;
    }
    let (data, sw) = response.split_at(response.len() - 2);
    Some((u16::from_be_bytes([sw[0], sw[1]]), data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_round_trip() {
        let apdu: Vec<u8> = (0..150u8).collect();
        let fs = frames(&apdu, Some(CHANNEL), HID_REPORT);
        assert!(fs.iter().all(|f| f.len() == 64));
        assert_eq!(&fs[0][..7], &[0x01, 0x01, 0x05, 0x00, 0x00, 0x00, 150]);
        assert_eq!(fs.len(), 3); // 152 bytes, 59 per frame
        // The device answers in the same framing.
        let mut r = Reader::new(Some(CHANNEL));
        let mut out = None;
        for f in &fs {
            out = r.push(f).unwrap();
        }
        assert_eq!(out.unwrap(), apdu);
    }

    #[test]
    fn ble_frames_are_unpadded_and_use_the_mtu() {
        let apdu = [0xe1, 0x05, 0x00, 0x01, 0x00];
        let fs = frames(&apdu, None, 153);
        assert_eq!(fs, vec![vec![0x05, 0x00, 0x00, 0x00, 0x05, 0xe1, 0x05, 0x00, 0x01, 0x00]]);
        let mut r = Reader::new(None);
        // A response: 4 bytes of data then 9000.
        let resp = [0x05, 0, 0, 0, 6, 0xde, 0xad, 0xbe, 0xef, 0x90, 0x00];
        let got = r.push(&resp).unwrap().unwrap();
        assert_eq!(split_status(&got), Some((0x9000, &[0xde, 0xad, 0xbe, 0xef][..])));
    }

    #[test]
    fn skips_other_channels_and_spots_gaps() {
        let mut r = Reader::new(Some(CHANNEL));
        assert_eq!(r.push(&[0xbf, 0xff, 0x05, 0, 0]), Err(FrameError::Ignore));
        assert_eq!(
            r.push(&[0x01, 0x01, 0x05, 0, 1, 0, 0]),
            Err(FrameError::Bad("frames out of order"))
        );
    }
}
