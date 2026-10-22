//! The 433 MHz remote protocol as the application sees it: what each button
//! means, how a press changes a slot, and the sequence that learns a new remote.
//!
//! Nothing here touches the radio directly - the button codes and the slot
//! bookkeeping are pure, which is what makes the pairing state machine easy to
//! reason about.

use zigbee_cluster_library::types::ZclEnum8;

use crate::config::save_config;
use crate::config::Slot;
use crate::state::mark_direction;
use crate::state::mark_fan_mode;
use crate::state::STATE;

pub const BTN_SPEED1: u8 = 0x2;
pub const BTN_SPEED2: u8 = 0x1;
pub const BTN_SPEED3: u8 = 0xC;
pub const BTN_SPEED4: u8 = 0x7;
pub const BTN_DIR_IN: u8 = 0x8;
pub const BTN_DIR_OUT: u8 = 0x4;
pub const BTN_DIR_TOGGLE: u8 = 0x3;
pub const BTN_POWER: u8 = 0xA;

pub fn speed_button(speed: u8) -> Option<u8> {
    match speed {
        1 => Some(BTN_SPEED1),
        2 => Some(BTN_SPEED2),
        3 => Some(BTN_SPEED3),
        4 => Some(BTN_SPEED4),
        _ => None,
    }
}

/// The learn sequence: speed 1, 2, 3, 4.
pub const PAIR_SEQ: [u8; 4] = [BTN_SPEED1, BTN_SPEED2, BTN_SPEED3, BTN_SPEED4];
/// Time allowed between consecutive learn presses.
pub const PAIR_STEP_MS: u64 = 15_000;
/// Whole pairing session timeout.
pub const PAIR_SESSION_MS: u64 = 90_000;
/// How long the outcome of a pairing attempt stays visible in the stage attribute.
pub const PAIR_RESULT_MS: u64 = 10_000;

/// What the pairing state machine is doing, as reported to the coordinator:
/// which speed press it is waiting for, or how the last attempt ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    WaitingSpeed1,
    WaitingSpeed2,
    WaitingSpeed3,
    WaitingSpeed4,
    Paired,
    Failed,
}

impl ZclEnum8 for Stage {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Stage::Idle),
            1 => Some(Stage::WaitingSpeed1),
            2 => Some(Stage::WaitingSpeed2),
            3 => Some(Stage::WaitingSpeed3),
            4 => Some(Stage::WaitingSpeed4),
            5 => Some(Stage::Paired),
            6 => Some(Stage::Failed),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            Stage::Idle => 0,
            Stage::WaitingSpeed1 => 1,
            Stage::WaitingSpeed2 => 2,
            Stage::WaitingSpeed3 => 3,
            Stage::WaitingSpeed4 => 4,
            Stage::Paired => 5,
            Stage::Failed => 6,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    Auto,
}

impl Direction {
    pub fn as_u8(self) -> u8 {
        match self {
            Direction::In => 0,
            Direction::Out => 1,
            Direction::Auto => 2,
        }
    }

    pub fn from_u8(value: u8) -> Direction {
        match value {
            1 => Direction::Out,
            2 => Direction::Auto,
            _ => Direction::In,
        }
    }
}

impl ZclEnum8 for Direction {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Direction::In),
            1 => Some(Direction::Out),
            2 => Some(Direction::Auto),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        self.as_u8()
    }
}

pub fn apply_button(slot: &mut Slot, button: u8) {
    match button {
        BTN_SPEED1 | BTN_SPEED2 | BTN_SPEED3 | BTN_SPEED4 => {
            slot.speed = match button {
                BTN_SPEED1 => 1,
                BTN_SPEED2 => 2,
                BTN_SPEED3 => 3,
                _ => 4,
            };
            slot.power = true;
        }
        BTN_DIR_IN => slot.dir = 0,
        BTN_DIR_OUT => slot.dir = 1,
        // the third direction button puts the fan in automatic mode, where it
        // alternates between the two directions on its own
        BTN_DIR_TOGGLE => slot.dir = 2,
        BTN_POWER => slot.power = !slot.power,
        _ => {}
    }
}

/// Button that selects a direction (0 in, 1 out, 2 auto).
pub fn dir_button(dir: u8) -> Option<u8> {
    match dir {
        0 => Some(BTN_DIR_IN),
        1 => Some(BTN_DIR_OUT),
        2 => Some(BTN_DIR_TOGGLE),
        _ => None,
    }
}

pub fn dir_name(dir: u8) -> &'static str {
    match dir {
        0 => "in",
        1 => "out",
        _ => "auto",
    }
}

/// Code we would send to make `slot` match its desired state.
pub fn code_for(slot: &Slot, button: u8) -> u32 {
    (slot.addr << 4) | u32::from(button)
}

pub enum Ev {
    None,
    Step(usize, u8),
    Paired(u32, usize),
    NoSlot(u32),
}

/// Called for every decoded frame (addr, button).
pub fn on_frame(addr: u32, button: u8, now_ms: u64) -> Ev {
    STATE.lock(|s| {
        let mut st = s.borrow_mut();

        // ---- pairing state machine
        if st.pairing {
            if st.pair_last_ms != 0 && now_ms.saturating_sub(st.pair_last_ms) > PAIR_STEP_MS {
                st.pair_step = 0; // gap too long, restart
            }
            if st.pair_addr != addr {
                st.pair_addr = addr;
                st.pair_step = 0;
            }
            if button == PAIR_SEQ[st.pair_step] {
                st.pair_step += 1;
                st.pair_last_ms = now_ms;
                if st.pair_step == PAIR_SEQ.len() {
                    // learned! find a free slot
                    let existing = st.cfg.slots.iter().position(|s| s.used() && s.addr == addr);
                    let free = st.cfg.slots.iter().position(|s| !s.used());
                    let slot = existing.or(free);
                    if let Some(i) = slot {
                        let name = st.cfg.slots[i].name;
                        st.cfg.slots[i] = Slot {
                            addr,
                            speed: 0,
                            dir: 0,
                            power: false,
                            name,
                        };
                        save_config(&st.cfg);
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
                        st.stage = Stage::Paired;
                        st.stage_until_ms = now_ms + PAIR_RESULT_MS;
                        return Ev::Paired(addr, i);
                    } else {
                        st.pairing = false;
                        st.pair_step = 0;
                        st.pair_addr = 0;
                        st.stage = Stage::Failed;
                        st.stage_until_ms = now_ms + PAIR_RESULT_MS;
                        return Ev::NoSlot(addr);
                    }
                }
            } else if button == PAIR_SEQ[0] {
                st.pair_step = 1;
                st.pair_last_ms = now_ms;
            } else {
                st.pair_step = 0;
            }
            return Ev::Step(st.pair_step, button);
        }

        // ---- normal operation: track state of a known remote
        let mut changed = false;
        for (i, sl) in st.cfg.slots.iter_mut().enumerate() {
            if sl.used() && sl.addr == addr {
                apply_button(sl, button);
                changed = true;
                // the coordinator should see what the remote just did
                mark_fan_mode(i);
                mark_direction(i);
                break;
            }
        }
        if changed {
            let snapshot = st.cfg;
            drop(st);
            save_config(&snapshot);
        }
        Ev::None
    })
}

