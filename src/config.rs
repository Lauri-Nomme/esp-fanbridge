//! Persistent configuration: the device name and the learned remotes with
//! their last known state, in one flash sector.
//!
//! The layout is versioned and older blobs are migrated on read, so a device
//! that is already paired survives a firmware update.

pub const CFG_ADDR: u32 = 0x003E_0000;
pub const CFG_SECTOR: u32 = CFG_ADDR / 0x1000;
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

    pub fn bytes(&self) -> [u8; CFG_BYTES] {
        let mut b = [0u8; CFG_BYTES];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        self.device_name.write(&mut b[DEVICE_NAME_OFF..][..NAME_MAX]);
        for (i, s) in self.slots.iter().enumerate() {
            let o = SLOTS_OFF + i * SLOT_SIZE;
            b[o..o + 4].copy_from_slice(&s.addr.to_le_bytes());
            b[o + 4] = s.speed;
            b[o + 5] = s.dir;
            b[o + 6] = u8::from(s.power);
            b[o + 7] = 0;
            s.name.write(&mut b[o + 8..][..NAME_MAX]);
        }
        b[CHECKSUM_OFF..CHECKSUM_OFF + 4].copy_from_slice(&self.checksum.to_le_bytes());
        b
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
        let b = self.bytes();
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

/// Read the config sector. Returns the empty config if nothing valid is stored.
pub fn load_config() -> Config {
    let mut buf = [0u32; CFG_BYTES / 4];
    let r = unsafe {
        esp_storage::ll::spiflash_read(
            CFG_ADDR,
            buf.as_mut_ptr(),
            core::mem::size_of_val(&buf) as u32,
        )
    };
    if r.is_err() {
        return Config::EMPTY;
    }
    let bytes: &[u8; CFG_BYTES] = unsafe { &*(buf.as_ptr() as *const [u8; CFG_BYTES]) };
    Config::from_bytes(bytes).unwrap_or(Config::EMPTY)
}

/// Erase the sector and write the config back.
pub fn save_config(cfg: &Config) -> bool {
    let mut c = *cfg;
    c.magic = CFG_MAGIC;
    c.version = CFG_VERSION;
    c.checksum = c.compute_checksum(CFG_VERSION);
    let mut buf = [0u32; CFG_BYTES / 4];
    {
        let bytes: &mut [u8; CFG_BYTES] = unsafe { &mut *(buf.as_mut_ptr() as *mut [u8; CFG_BYTES]) };
        *bytes = c.bytes();
    }
    critical_section::with(|_| unsafe {
        let _ = esp_storage::ll::spiflash_unlock();
        if esp_storage::ll::spiflash_erase_sector(CFG_SECTOR).is_err() {
            return false;
        }
        esp_storage::ll::spiflash_write(
            CFG_ADDR,
            buf.as_ptr(),
            core::mem::size_of_val(&buf) as u32,
        )
        .is_ok()
    })
}

