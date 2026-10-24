//! Persistent configuration: the flash sector that holds the device name and
//! the learned remotes with their last known state.
//!
//! The byte format itself lives in `esp-fanbridge-protocol` so that the
//! version-1 migration - the part that can silently lose a user's paired
//! remotes - is covered by host tests. This module is the flash I/O around it.

pub use esp_fanbridge_protocol::config::{Config, Name, Slot, CFG_BYTES, NAME_MAX};

pub const CFG_ADDR: u32 = 0x003E_0000;
pub const CFG_SECTOR: u32 = CFG_ADDR / 0x1000;

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
    let mut buf = [0u32; CFG_BYTES / 4];
    {
        let bytes: &mut [u8; CFG_BYTES] = unsafe { &mut *(buf.as_mut_ptr() as *mut [u8; CFG_BYTES]) };
        // `bytes` always stamps the current version and its checksum
        *bytes = cfg.bytes();
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
