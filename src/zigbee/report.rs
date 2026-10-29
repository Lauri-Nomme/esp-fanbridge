//! Attribute reporting: telling the coordinator what changed.
//!
//! Reports are built and sent from a task of their own (see
//! [`crate::zigbee::report_task`]) because a send waits for an APS
//! acknowledgement; doing that inline would stall RF reception.

use core::sync::atomic::Ordering;

use embassy_time::Delay as AsyncDelay;
use zigbee_cluster_library::profile;
use zigbee_cluster_library::reporting::AttributeReportBuilder;
use zigbee_cluster_library::sender::ZclReportTarget;
use zigbee_cluster_library::sender::ZclSender;
use zigbee_cluster_library::types::Uint8;
use zigbee_cluster_library::types::Uint16;
use zigbee_cluster_library::types::ZclEnum8;

use crate::pairing::Direction;
use crate::pairing::Stage;
use crate::state::REARM_COUNT;
use crate::state::STATE;
use crate::state::tx_dropped;
use crate::zigbee::EP_FAN0;
use crate::zigbee::EP_MASTER;
use crate::zigbee::ZbStack;
use crate::zigbee::clusters::*;

/// Why a report attempt did not reach the coordinator.
///
/// The distinction matters: `NoAck` means the frame went out and nobody
/// acknowledged it (link or security), `SecurityFail` means it never got that
/// far, and the rest are addressing or framing mistakes on our side.
#[derive(Clone, Copy)]
pub enum ReportError {
    /// The frame could not be built (should never happen).
    Build,
    /// The send returned an error before a confirm arrived.
    Send,
    /// The coordinator answered, but not with success.
    Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus),
}

impl ReportError {
    pub fn name(&self) -> &'static str {
        match self {
            ReportError::Build => "build",
            ReportError::Send => "send",
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::Success) => "success?",
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::NoShortAddress) => {
                "NoShortAddress"
            }
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::NoBoundDevice) => {
                "NoBoundDevice"
            }
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::SecurityFail) => {
                "SecurityFail"
            }
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::NoAck) => "NoAck",
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::AsduTooLong) => {
                "AsduTooLong"
            }
            ReportError::Confirm(zigbee::aps::apsde::ApsdeSapConfirmStatus::Unsupported) => {
                "Unsupported"
            }
        }
    }
}

pub async fn send_state_report(stack: &ZbStack, bit: u32) -> Result<(), ReportError> {
    use core::sync::atomic::AtomicU8;
    static SEQ: AtomicU8 = AtomicU8::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let mut buf = [0u8; 32];

    let (src_endpoint, frame) = if bit < 8 {
        let slot = bit as usize;
        let (used, mode) = STATE.lock(|s| {
            let st = s.borrow();
            let sl = &st.cfg.slots[slot];
            (
                sl.used(),
                if sl.power {
                    mode_of_speed(sl.speed)
                } else {
                    FanMode::Off
                },
            )
        });
        if !used {
            return Ok(()); // nothing behind this bit, drop it
        }
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_FANMODE, mode))
            .and_then(AttributeReportBuilder::finish)
        else {
            return Err(ReportError::Build);
        };
        (EP_FAN0 + slot as u8, frame)
    } else if bit < 16 {
        let slot = (bit - 8) as usize;
        let (used, dir) = STATE.lock(|s| {
            let st = s.borrow();
            (
                st.cfg.slots[slot].used(),
                Direction::from_u8(st.cfg.slots[slot].dir),
            )
        });
        if !used {
            return Ok(());
        }
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_DIRECTION, dir))
            .and_then(AttributeReportBuilder::finish)
        else {
            return Err(ReportError::Build);
        };
        (EP_FAN0 + slot as u8, frame)
    } else {
        let pairing = STATE.lock(|s| s.borrow().pairing);
        let Ok(frame) = AttributeReportBuilder::new(&mut buf, seq)
            .and_then(|b| b.push(&A_ONOFF, pairing))
            .and_then(AttributeReportBuilder::finish)
        else {
            return Err(ReportError::Build);
        };
        (EP_MASTER, frame)
    };

    match stack
        .device()
        .send_attribute_report(
            ZclReportTarget {
                dst_short: 0,
                src_endpoint,
                dst_endpoint: 1,
                profile_id: profile::HOME_AUTOMATION,
            },
            frame,
            &mut AsyncDelay,
        )
        .await
    {
        Ok(confirm) if confirm.status == zigbee::aps::apsde::ApsdeSapConfirmStatus::Success => {
            Ok(())
        }
        Ok(confirm) => Err(ReportError::Confirm(confirm.status)),
        Err(_) => Err(ReportError::Send),
    }
}

/// Send the pairing stage + paired count to the coordinator.
///
/// Returns whether it was accepted, so a failed attempt can be retried instead
/// of leaving the coordinator with a stale value forever.
pub async fn send_pairing_report(stack: &ZbStack) -> Result<(), ReportError> {
    use core::sync::atomic::AtomicU8;
    static SEQ: AtomicU8 = AtomicU8::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let stage = Stage::from_raw(STAGE_NOW.load(Ordering::Relaxed) as u8).unwrap_or(Stage::Idle);
    let (count, exposed) = STATE.lock(|s| {
        let st = s.borrow();
        (st.paired_count(), st.exposed_count())
    });
    let rearms = REARM_COUNT.load(Ordering::Relaxed) as u16;
    let dropped = tx_dropped() as u16;
    let mut buf = [0u8; 40];
    let Ok(report) = AttributeReportBuilder::new(&mut buf, seq)
        .and_then(|b| b.push(&A_STAGE, stage))
        .and_then(|b| b.push(&A_PAIRED_COUNT, Uint8(count)))
        .and_then(|b| b.push(&A_EXPOSED_COUNT, Uint8(exposed)))
        .and_then(|b| b.push(&A_RESTART_REQUIRED, count != exposed))
        .and_then(|b| b.push(&A_RX_REARM_COUNT, Uint16(rearms)))
        // the only silent failure this device has: a command that never reached
        // the fan. Reported so Home Assistant shows it without a console.
        .and_then(|b| b.push(&A_TX_DROPPED, Uint16(dropped)))
        .and_then(AttributeReportBuilder::finish)
    else {
        return Err(ReportError::Build);
    };
    match stack
        .device()
        .send_attribute_report(
            ZclReportTarget {
                dst_short: 0,
                src_endpoint: EP_MASTER,
                dst_endpoint: 1,
                profile_id: profile::HOME_AUTOMATION,
            },
            report,
            &mut AsyncDelay,
        )
        .await
    {
        Ok(confirm) if confirm.status == zigbee::aps::apsde::ApsdeSapConfirmStatus::Success => {
            Ok(())
        }
        Ok(confirm) => Err(ReportError::Confirm(confirm.status)),
        Err(_) => Err(ReportError::Send),
    }
}
