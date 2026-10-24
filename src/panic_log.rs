//! The last panic, persisted across the reset it caused.
//!
//! A device that lives in a cabinet and quietly reboots tells you nothing: the
//! serial console is gone by the time anyone looks. The panic handler writes one
//! short line to a flash sector, and the next boot prints it and clears it, so
//! the first console line after a crash is the reason for it.

use core::fmt::Write;

use esp_storage::ll::spiflash_erase_sector;
use esp_storage::ll::spiflash_read;
use esp_storage::ll::spiflash_unlock;
use esp_storage::ll::spiflash_write;

/// One sector of its own, next to the config sector and clear of the stack's
/// flash; see `docs/DESIGN.md` for the layout.
pub const PANIC_ADDR: u32 = 0x003D_0000;
pub const PANIC_SECTOR: u32 = PANIC_ADDR / 0x1000;

const MAGIC: u32 = 0x5041_4E31; // "PAN1"
/// Room for `panicked at <file>:<line>:<col>:` and the message, truncated.
pub const TEXT_MAX: usize = 160;
const LEN_OFF: usize = 8;
const TEXT_OFF: usize = 12;
const SUM_OFF: usize = TEXT_OFF + TEXT_MAX;
const RECORD: usize = SUM_OFF + 4;

struct Bounded<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Write for Bounded<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.buf.len() - self.pos;
        let take = s.len().min(room);
        // never split a character: the text is printed back as UTF-8
        let mut cut = take;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.buf[self.pos..self.pos + cut].copy_from_slice(&s.as_bytes()[..cut]);
        self.pos += cut;
        if cut < s.len() {
            return Err(core::fmt::Error);
        }
        Ok(())
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, v) in bytes.iter().enumerate() {
        sum = sum.wrapping_mul(31).wrapping_add(u32::from(*v) + i as u32);
    }
    sum ^ 0x5AA5_5AA5
}

/// Write a panic description to flash. Best effort: a failure here must not
/// change how the panic itself is reported.
pub fn record(uptime_ms: u32, info: &core::panic::PanicInfo<'_>) {
    let mut text = [0u8; TEXT_MAX];
    let len = {
        let mut w = Bounded {
            buf: &mut text,
            pos: 0,
        };
        // Display for PanicInfo is "panicked at <location>: <message>"
        let _ = write!(w, "{info}");
        w.pos
    };

    let mut rec = [0u8; RECORD];
    rec[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    rec[4..8].copy_from_slice(&uptime_ms.to_le_bytes());
    rec[LEN_OFF..LEN_OFF + 2].copy_from_slice(&(len as u16).to_le_bytes());
    rec[TEXT_OFF..TEXT_OFF + len].copy_from_slice(&text[..len]);
    let sum = checksum(&rec[..SUM_OFF]);
    rec[SUM_OFF..].copy_from_slice(&sum.to_le_bytes());

    // the record is 4-byte aligned by construction (offsets 0,4,8,12,172)
    let mut words = [0u32; RECORD / 4];
    for (i, chunk) in rec.as_chunks::<4>().0.iter().enumerate() {
        words[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    critical_section::with(|_| unsafe {
        let _ = spiflash_unlock();
        if spiflash_erase_sector(PANIC_SECTOR).is_err() {
            return;
        }
        let _ = spiflash_write(PANIC_ADDR, words.as_ptr(), RECORD as u32);
    });
}

/// Read the stored panic, if there is one.
///
/// Reading does not write; [`clear`] does that, from the main loop. Erasing
/// during early boot, before anything else has run, aborts the device on this
/// chip - the same erase is unremarkable once the scheduler is running.
pub fn take(out: &mut [u8; TEXT_MAX]) -> Option<(u32, usize)> {
    let mut words = [0u32; RECORD / 4];
    let r = unsafe { spiflash_read(PANIC_ADDR, words.as_mut_ptr(), RECORD as u32) };
    if r.is_err() {
        return None;
    }
    let mut rec = [0u8; RECORD];
    for (i, w) in words.iter().enumerate() {
        rec[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    let magic = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
    if magic != MAGIC {
        return None;
    }
    let uptime = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
    let len = u16::from_le_bytes([rec[LEN_OFF], rec[LEN_OFF + 1]]) as usize;
    let sum = u32::from_le_bytes([
        rec[SUM_OFF],
        rec[SUM_OFF + 1],
        rec[SUM_OFF + 2],
        rec[SUM_OFF + 3],
    ]);
    if len > TEXT_MAX || checksum(&rec[..SUM_OFF]) != sum {
        return None;
    }
    out[..len].copy_from_slice(&rec[TEXT_OFF..TEXT_OFF + len]);
    Some((uptime, len))
}

/// Erase the record, so the next boot does not report the same crash again.
pub fn clear() {
    let _ = critical_section::with(|_| unsafe {
        let _ = spiflash_unlock();
        spiflash_erase_sector(PANIC_SECTOR)
    });
}
