//! The persistent configuration *format*: the device name and the learned
//! remotes with their last known state.
//!
//! This is the byte layout only - reading and writing the flash sector lives in
//! the firmware. It sits here so the migration from the older layout, which is
//! the part that can silently lose a user's paired remotes, is covered by tests.

pub const CFG_MAGIC: u32 = 0x4641_4E31; // "FAN1"
/// Bumped whenever the layout below changes; older blobs are migrated.
pub const CFG_VERSION: u32 = 2;
/// Longest name (device or fan) accepted by the console.
pub const NAME_MAX: usize = 24;
/// Bytes written per save; the sector is 4 KiB, only the used part is written.
pub const CFG_BYTES: usize = 512;

/// A user-facing name: UTF-8, at most [`NAME_MAX`] bytes, zero-padded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    buf: [u8; NAME_MAX],
    len: u8,
}

impl Name {
    pub const EMPTY: Name = Name {
        buf: [0; NAME_MAX],
        len: 0,
    };

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns false (and leaves the name untouched) if it does not fit.
    pub fn set(&mut self, s: &str) -> bool {
        let bytes = s.as_bytes();
        if bytes.len() > NAME_MAX || core::str::from_utf8(bytes).is_err() {
            return false;
        }
        self.buf[..bytes.len()].copy_from_slice(bytes);
        self.buf[bytes.len()..].fill(0);
        self.len = bytes.len() as u8;
        true
    }

    pub fn write(&self, out: &mut [u8]) {
        out[..NAME_MAX].copy_from_slice(&self.buf);
    }

    pub fn read(input: &[u8]) -> Name {
        let mut name = Name::EMPTY;
        let len = input.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        if core::str::from_utf8(&input[..len]).is_ok() {
            name.buf[..len].copy_from_slice(&input[..len]);
            name.len = len as u8;
        }
        name
    }
}

impl Default for Name {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Clone, Copy, Default)]
pub struct Slot {
    /// 20-bit device address; 0 means empty
    pub addr: u32,
    /// 0 = unknown/off, 1..=4
    pub speed: u8,
    /// 0 = in, 1 = out
    pub dir: u8,
    pub power: bool,
    /// user-facing name, empty when unset
    pub name: Name,
}

impl Slot {
    pub fn used(&self) -> bool {
        self.addr != 0
    }
}

/// On-flash layout (version 2, little-endian):
///
/// ```text
///   0..4    magic
///   4..8    version
///   8..32   device name (zero padded)
///   32..    slots, 32 bytes each: addr u32, speed u8, dir u8, power u8, pad u8,
///           name (24 bytes)
///   end..   checksum u32
/// ```
pub const DEVICE_NAME_OFF: usize = 8;
pub const SLOTS_OFF: usize = DEVICE_NAME_OFF + NAME_MAX;
pub const SLOT_SIZE: usize = 32;
pub const CHECKSUM_OFF: usize = SLOTS_OFF + 8 * SLOT_SIZE;

#[derive(Clone, Copy)]
pub struct Config {
    pub magic: u32,
    pub version: u32,
    pub device_name: Name,
    pub slots: [Slot; 8],
    pub checksum: u32,
}

impl Config {
    pub const EMPTY: Config = Config {
        magic: CFG_MAGIC,
        version: CFG_VERSION,
        device_name: Name::EMPTY,
        slots: [Slot {
            addr: 0,
            speed: 0,
            dir: 0,
            power: false,
            name: Name::EMPTY,
        }; 8],
        checksum: 0,
    };

    /// Serialise a loadable image: always in the current layout, with the
    /// current version and its checksum, so a caller cannot accidentally write
    /// a blob that [`Self::from_bytes`] rejects.
    pub fn bytes(&self) -> [u8; CFG_BYTES] {
        self.serialise(CFG_VERSION, self.compute_checksum(CFG_VERSION))
    }

    fn serialise(&self, version: u32, checksum: u32) -> [u8; CFG_BYTES] {
        let mut b = [0u8; CFG_BYTES];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..8].copy_from_slice(&version.to_le_bytes());
        self.device_name
            .write(&mut b[DEVICE_NAME_OFF..][..NAME_MAX]);
        for (i, s) in self.slots.iter().enumerate() {
            let o = SLOTS_OFF + i * SLOT_SIZE;
            // the format only carries 20 address bits, and the checksum is
            // computed over what was written
            b[o..o + 4].copy_from_slice(&(s.addr & 0x000F_FFFF).to_le_bytes());
            b[o + 4] = s.speed;
            b[o + 5] = s.dir;
            b[o + 6] = u8::from(s.power);
            b[o + 7] = 0;
            s.name.write(&mut b[o + 8..][..NAME_MAX]);
        }
        b[CHECKSUM_OFF..CHECKSUM_OFF + 4].copy_from_slice(&checksum.to_le_bytes());
        b
    }

    /// How many remotes are paired (slots with a device address).
    pub fn paired_count(&self) -> u8 {
        self.slots.iter().filter(|s| s.used()).count() as u8
    }

    pub fn from_bytes(b: &[u8; CFG_BYTES]) -> Option<Config> {
        let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if magic != CFG_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);

        // version 1 kept the slots right after the header (8 bytes each, no
        // names) and its checksum at 72; version 2 moved both.
        let (slots_off, slot_size, checksum_off) = if version >= 2 {
            (SLOTS_OFF, SLOT_SIZE, CHECKSUM_OFF)
        } else {
            (8, 8, 72)
        };

        let mut c = Config {
            magic,
            version,
            device_name: Name::EMPTY,
            slots: [Slot::default(); 8],
            checksum: 0,
        };
        if version >= 2 {
            c.device_name = Name::read(&b[DEVICE_NAME_OFF..][..NAME_MAX]);
        }
        for (i, s) in c.slots.iter_mut().enumerate() {
            let o = slots_off + i * slot_size;
            s.addr = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) & 0x000F_FFFF;
            s.speed = b[o + 4];
            s.dir = b[o + 5];
            s.power = b[o + 6] != 0;
            if version >= 2 {
                s.name = Name::read(&b[o + 8..][..NAME_MAX]);
            }
        }

        let stored = u32::from_le_bytes([
            b[checksum_off],
            b[checksum_off + 1],
            b[checksum_off + 2],
            b[checksum_off + 3],
        ]);
        if c.compute_checksum(version) != stored {
            return None;
        }
        c.checksum = stored;
        c.version = CFG_VERSION;
        Some(c)
    }

    /// The checksum covers everything before it; a version-1 blob has a
    /// different layout, so it is checksummed the way it was written.
    pub fn compute_checksum(&self, version: u32) -> u32 {
        if version < 2 {
            // rebuild the version-1 image: header, 8-byte slots, then the sum
            let mut b = [0u8; 128];
            b[0..4].copy_from_slice(&self.magic.to_le_bytes());
            b[4..8].copy_from_slice(&version.to_le_bytes());
            for (i, s) in self.slots.iter().enumerate() {
                let o = 8 + i * 8;
                b[o..o + 4].copy_from_slice(&s.addr.to_le_bytes());
                b[o + 4] = s.speed;
                b[o + 5] = s.dir;
                b[o + 6] = u8::from(s.power);
            }
            return checksum(&b[..72]);
        }
        let b = self.serialise(version, 0);
        checksum(&b[..CHECKSUM_OFF])
    }
}

pub fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, v) in bytes.iter().enumerate() {
        sum = sum.wrapping_mul(31).wrapping_add(u32::from(*v) + i as u32);
    }
    sum ^ 0xA5A5_5A5A
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let mut c = Config::EMPTY;
        assert!(c.device_name.set("Ceiling fans"));
        c.slots[0].addr = 0x191D0;
        c.slots[0].speed = 3;
        c.slots[0].dir = 2; // auto
        c.slots[0].power = true;
        assert!(c.slots[0].name.set("lr-r"));
        c.slots[2].addr = 0x1A5F0;
        c.slots[2].speed = 4;
        c
    }

    #[test]
    fn version_two_round_trips() {
        let c = sample();
        let image = c.bytes();
        let parsed = Config::from_bytes(&image).expect("valid image");
        assert_eq!(parsed.version, CFG_VERSION);
        assert_eq!(parsed.device_name.as_str(), "Ceiling fans");
        assert_eq!(parsed.slots[0].addr, 0x191D0);
        assert_eq!(parsed.slots[0].speed, 3);
        assert_eq!(parsed.slots[0].dir, 2);
        assert!(parsed.slots[0].power);
        assert_eq!(parsed.slots[0].name.as_str(), "lr-r");
        assert!(!parsed.slots[1].used());
        assert_eq!(parsed.slots[2].addr, 0x1A5F0);
        assert_eq!(parsed.paired_count(), 2);
    }

    /// The layout the first firmware wrote: no names, 8-byte slots, checksum at
    /// 72. A device paired with that build must keep its remotes.
    #[test]
    fn version_one_migrates() {
        let mut old = [0u8; 128];
        old[0..4].copy_from_slice(&CFG_MAGIC.to_le_bytes());
        old[4..8].copy_from_slice(&1u32.to_le_bytes());
        let slots = [
            (0x191D0u32, 3u8, 0u8, 1u8),
            (0x06800, 1, 1, 0),
            (0, 0, 0, 0),
        ];
        for (i, (addr, speed, dir, power)) in slots.iter().enumerate() {
            let o = 8 + i * 8;
            old[o..o + 4].copy_from_slice(&addr.to_le_bytes());
            old[o + 4] = *speed;
            old[o + 5] = *dir;
            old[o + 6] = *power;
        }
        let sum = checksum(&old[..72]);
        old[72..76].copy_from_slice(&sum.to_le_bytes());

        let mut image = [0u8; CFG_BYTES];
        image[..128].copy_from_slice(&old);
        let parsed = Config::from_bytes(&image).expect("version 1 image must be accepted");
        assert_eq!(parsed.version, CFG_VERSION);
        assert_eq!(parsed.slots[0].addr, 0x191D0);
        assert_eq!(parsed.slots[0].speed, 3);
        assert!(parsed.slots[0].power);
        assert_eq!(parsed.slots[1].addr, 0x06800);
        assert_eq!(parsed.slots[1].dir, 1);
        // names did not exist in version 1, so they come up empty
        assert!(parsed.slots[0].name.is_empty());
        assert!(parsed.device_name.is_empty());
    }

    #[test]
    fn rejects_a_foreign_or_corrupt_image() {
        let mut image = sample().bytes();
        image[0] ^= 0xFF; // magic
        assert!(Config::from_bytes(&image).is_none());

        let mut image = sample().bytes();
        image[40] ^= 0x01; // inside the first slot's address
        assert!(Config::from_bytes(&image).is_none());

        let mut image = sample().bytes();
        image[CHECKSUM_OFF] ^= 0xFF;
        assert!(Config::from_bytes(&image).is_none());
    }

    #[test]
    fn empty_config_is_valid_and_reads_back_empty() {
        let parsed = Config::from_bytes(&Config::EMPTY.bytes()).expect("valid");
        assert_eq!(parsed.paired_count(), 0);
        assert!(parsed.device_name.is_empty());
    }

    #[test]
    fn names_are_bounded_and_zero_padded() {
        let mut name = Name::EMPTY;
        assert!(name.set("kitchen"));
        assert_eq!(name.as_str(), "kitchen");
        assert!(!name.set(&"x".repeat(NAME_MAX + 1)));
        assert_eq!(
            name.as_str(),
            "kitchen",
            "a rejected name must not change the old one"
        );
    }

    #[test]
    fn slot_addresses_are_masked_to_twenty_bits() {
        let mut c = Config::EMPTY;
        c.slots[0].addr = 0xFFFF_FFFF;
        let parsed = Config::from_bytes(&c.bytes()).expect("valid");
        assert_eq!(parsed.slots[0].addr, 0x000F_FFFF);
    }
}
