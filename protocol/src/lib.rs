//! The parts of esp-fanbridge that are pure logic.
//!
//! Everything in here is independent of the ESP32 and the radio, which is the
//! point: these are the pieces that broke silently in the past (the 49-byte
//! packet expansion and the frame decoder), and here they can be tested with
//! `cargo test` on a workstation instead of on a ceiling fan.

#![no_std]

pub mod config;

/// Ignore edges shorter than this (demodulator noise).
pub const NOISE_US: u32 = 100;
/// Silence that ends a frame.
pub const FRAME_GAP_US: u32 = 6_000;
/// Symbol split / sanity bounds.
pub const SHORT_MIN: u32 = 250;
pub const SHORT_MAX: u32 = 650;
pub const LONG_MIN: u32 = 850;
pub const LONG_MAX: u32 = 1800;
pub const MAX_SEG: usize = 80;

/// One run-length of the demodulated OOK envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub dt: u32,
    pub high: bool,
}

/// Classify one pulse width: `false` = short, `true` = long.
pub fn sym(dt: u32) -> Option<bool> {
    if (SHORT_MIN..=SHORT_MAX).contains(&dt) {
        Some(false)
    } else if (LONG_MIN..=LONG_MAX).contains(&dt) {
        Some(true)
    } else {
        None
    }
}

/// Decode a 24-bit code out of a run-length sequence, if one is in there.
///
/// The frame is 24 symbols of one high and one low pulse each, ending with a
/// short pulse; the total length is checked so noise cannot masquerade as a
/// frame.
pub fn decode_frame(segs: &[Seg]) -> Option<u32> {
    for start in 0..segs.len() {
        if !segs[start].high || start + 48 > segs.len() {
            continue;
        }
        let mut bits: u32 = 0;
        let mut ok = true;
        for i in 0..24 {
            let h = segs[start + 2 * i];
            let l = segs[start + 2 * i + 1];
            if !h.high || l.high {
                ok = false;
                break;
            }
            let (hs, ls) = match (sym(h.dt), sym(l.dt)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    ok = false;
                    break;
                }
            };
            if hs == ls {
                ok = false;
                break;
            }
            bits = (bits << 1) | u32::from(hs);
        }
        if ok {
            let total: u32 = segs[start..start + 48].iter().map(|s| s.dt).sum();
            if (33_000..=45_000).contains(&total) {
                return Some(bits);
            }
        }
    }
    None
}

/// Bit period of the expanded stream: 100 us, so the protocol's 400/1200 us
/// pulses are exactly 4/12 bits and the on-air timing matches the bit-banged
/// version.
pub const TX_BIT_US: u32 = 100;
/// 24 symbols * 16 bits + the 4-bit trailing pulse, padded to whole bytes.
pub const TX_PACKET_LEN: u8 = 49;

/// Set one bit of a big-endian bit buffer.
fn put_bit(out: &mut [u8], pos: &mut usize, one: bool) {
    if one {
        out[*pos / 8] |= 1 << (7 - *pos % 8);
    }
    *pos += 1;
}

/// Expand a 24-bit remote code into the NRZ bitstream the radio can send.
///
/// `0` is 400 us high + 1200 us low (4 on, 12 off), `1` is 1200 us high +
/// 400 us low (12 on, 4 off), followed by the trailing short pulse. See
/// `docs/PACKET-TX.md`.
pub fn expand_code(code: u32) -> [u8; TX_PACKET_LEN as usize] {
    let mut out = [0u8; TX_PACKET_LEN as usize];
    let mut pos = 0usize;
    for i in (0..24).rev() {
        let (high, low) = if (code >> i) & 1 == 1 {
            (12, 4)
        } else {
            (4, 12)
        };
        for _ in 0..high {
            put_bit(&mut out, &mut pos, true);
        }
        for _ in 0..low {
            put_bit(&mut out, &mut pos, false);
        }
    }
    for _ in 0..4 {
        put_bit(&mut out, &mut pos, true);
    }
    out
}

/// Turn an expanded bitstream back into run lengths, as the receiver would see
/// it. Used by the tests to close the loop between `expand_code` and
/// `decode_frame`.
pub fn segments_of_expanded(frame: &[u8]) -> ([Seg; MAX_SEG], usize) {
    let mut segs = [Seg { dt: 0, high: false }; MAX_SEG];
    let mut n = 0usize;
    let mut bit = 0usize;
    while bit < frame.len() * 8 {
        let one = frame[bit / 8] & (1 << (7 - bit % 8)) != 0;
        let mut len = 1usize;
        while bit + len < frame.len() * 8 {
            let next = frame[(bit + len) / 8] & (1 << (7 - (bit + len) % 8)) != 0;
            if next != one {
                break;
            }
            len += 1;
        }
        if n < MAX_SEG {
            segs[n] = Seg {
                dt: len as u32 * TX_BIT_US,
                high: one,
            };
            n += 1;
        }
        bit += len;
    }
    (segs, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from docs/PACKET-TX.md.
    #[test]
    fn expands_the_documented_example() {
        let frame = expand_code(0x068002);
        assert_eq!(frame.len(), 49);
        assert_eq!(
            &frame[..8],
            &[0xF0, 0x00, 0xF0, 0x00, 0xF0, 0x00, 0xF0, 0x00]
        );
        // 24 symbols * 16 bits + 4 trailing bits = 392 bits = 49 bytes exactly
        assert_eq!(TX_PACKET_LEN, 49);
    }

    #[test]
    fn first_symbol_follows_the_most_significant_bit() {
        // bit 23 set -> 12 on + 4 off
        assert_eq!(expand_code(0x80_0000)[0], 0xFF);
        assert_eq!(expand_code(0x80_0000)[1], 0xF0);
        // bit 23 clear -> 4 on + 12 off
        assert_eq!(expand_code(0x00_0000)[0], 0xF0);
        assert_eq!(expand_code(0x00_0000)[1], 0x00);
    }

    #[test]
    fn expansion_round_trips_through_the_decoder() {
        for code in [
            0x068002u32,
            0x191D07,
            0x1A5F0A,
            0x123457,
            0xFFFFFF,
            0x000001,
        ] {
            let (segs, n) = segments_of_expanded(&expand_code(code));
            assert_eq!(decode_frame(&segs[..n]), Some(code), "code 0x{code:06X}");
        }
    }

    #[test]
    fn short_and_long_are_classified() {
        assert_eq!(sym(400), Some(false));
        assert_eq!(sym(1200), Some(true));
        assert_eq!(sym(100), None);
        assert_eq!(sym(2000), None);
    }

    /// A genuine capture of the lr-r remote's speed-4 button, taken with the
    /// second C6+CC1101 listener at 8 us resolution (see docs/PROTOCOL.md).
    #[test]
    fn a_real_capture_decodes() {
        let edges: [u32; 49] = [
            378, 1243, 348, 1258, 354, 1237, 1161, 423, 1184, 418, 388, 1221, 406, 1190, 1202, 406,
            397, 1200, 403, 1215, 397, 1209, 1181, 424, 1178, 403, 1218, 387, 419, 1196, 1206, 403,
            403, 1187, 412, 1197, 406, 1191, 406, 1215, 381, 1218, 1196, 397, 1199, 406, 1215, 391,
            397,
        ];
        let mut segs = [Seg { dt: 0, high: false }; MAX_SEG];
        for (i, dt) in edges.iter().enumerate() {
            segs[i] = Seg {
                dt: *dt,
                high: i % 2 == 0,
            };
        }
        assert_eq!(decode_frame(&segs[..49]), Some(0x191D07));
    }

    #[test]
    fn noise_is_not_a_frame() {
        let segs = [Seg {
            dt: 120,
            high: true,
        }; 60];
        assert_eq!(decode_frame(&segs), None);
    }
}
