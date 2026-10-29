//! One type per ZCL cluster this device serves.
//!
//! Each server owns the attributes of its cluster and translates between ZCL
//! values and the application state; the dispatching (which endpoint belongs to
//! which slot) lives in [`crate::zigbee`].

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

use zigbee::zdo::ClusterReply;
use zigbee::zdo::ClusterRequest;
use zigbee::zdo::ClusterRequestHandler;
use zigbee_cluster_library::clusters::general::basic;
use zigbee_cluster_library::clusters::general::identify::IdentifyServer;
use zigbee_cluster_library::frame::Status;
use zigbee_cluster_library::reporting::AttributeReporting;
use zigbee_cluster_library::reporting::ReportingTable;
use zigbee_cluster_library::server::ClusterCommand;
use zigbee_cluster_library::server::ClusterServer;
use zigbee_cluster_library::server::CommandOutcome;
use zigbee_cluster_library::types::AttrInfo;
use zigbee_cluster_library::types::Attribute;
use zigbee_cluster_library::types::AttributeId;
use zigbee_cluster_library::types::Bool;
use zigbee_cluster_library::types::Cluster;
use zigbee_cluster_library::types::ClusterId;
use zigbee_cluster_library::types::Enum8;
use zigbee_cluster_library::types::ReadOnly;
use zigbee_cluster_library::types::ReadWrite;
use zigbee_cluster_library::types::Reportable;
use zigbee_cluster_library::types::ShortText;
use zigbee_cluster_library::types::TypeId;
use zigbee_cluster_library::types::Uint8;
use zigbee_cluster_library::types::Uint16;
use zigbee_cluster_library::types::ZclEnum8;
use zigbee_cluster_library::types::strings::ShortStr;

use crate::config::save_config;
use crate::pairing::Direction;
use crate::pairing::Stage;
use crate::pairing::*;
use crate::state::REARM_COUNT;
use crate::state::STATE;
use crate::state::mark_direction;
use crate::state::mark_fan_mode;
use crate::state::mark_pairing_switch;
use crate::state::request_reboot_in;
use crate::state::tx_dropped;
use crate::state::tx_push_for;
use crate::state::zb_event;
use crate::zigbee::EP_FAN0;
use crate::zigbee::EP_MASTER;

pub const CL_ONOFF: Cluster = Cluster::new(ClusterId(0x0006), "On/Off");
pub const CL_FAN: Cluster = Cluster::new(ClusterId(0x0202), "Fan Control");
/// Manufacturer-specific status cluster (no standard one carries this).
pub const CL_PAIRING: Cluster = Cluster::new(ClusterId(0xFC00), "espPairing");
/// Manufacturer-specific direction cluster: ZCL has no three-state direction.
pub const CL_DIRECTION: Cluster = Cluster::new(ClusterId(0xFC01), "espDirection");
pub const A_ONOFF: Attribute<Bool, ReadOnly, Reportable> =
    CL_ONOFF.attribute(AttributeId(0x0000), "OnOff");
pub const A_FANMODE: Attribute<Enum8<FanMode>, ReadWrite, Reportable> =
    CL_FAN.attribute(AttributeId(0x0000), "FanMode");
/// Required by the cluster, and the only way a coordinator can learn which modes
/// this fan actually cycles through.
pub const A_FAN_MODE_SEQ: Attribute<Enum8<FanModeSeq>, ReadOnly, Reportable> =
    CL_FAN.attribute(AttributeId(0x0001), "FanModeSequence");
pub const A_STAGE: Attribute<Enum8<Stage>, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0000), "stage");
pub const A_PAIRED_COUNT: Attribute<Uint8, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0001), "pairedCount");
pub const A_DIRECTION: Attribute<Enum8<Direction>, ReadWrite, Reportable> =
    CL_DIRECTION.attribute(AttributeId(0x0000), "direction");
pub const A_EXPOSED_COUNT: Attribute<Uint8, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0002), "exposedCount");
pub const A_RESTART_REQUIRED: Attribute<Bool, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0003), "restartRequired");
/// How often the receiver watchdog had to re-arm the radio (see `status`).
pub const A_RX_REARM_COUNT: Attribute<Uint16, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0004), "rxRearmCount");
/// Commands the transmit queue had to throw away because it was full (backlog
/// #54). Non-zero means a command never reached the fan, and the fan's state is
/// not what the device says it is - the only silent failure this device has, so
/// it is the one number worth watching from Home Assistant.
pub const A_TX_DROPPED: Attribute<Uint16, ReadOnly, Reportable> =
    CL_PAIRING.attribute(AttributeId(0x0005), "txDropped");
pub static ONOFF_ATTRS: [AttrInfo; 1] = [A_ONOFF.attr_info()];
/// Shared reporting store: enough to accept Configure Reporting from the coordinator.
pub static REPORTING: ReportingTable<16> = ReportingTable::new();
pub static FAN_ATTRS: [AttrInfo; 2] = [A_FANMODE.attr_info(), A_FAN_MODE_SEQ.attr_info()];
pub static PAIRING_ATTRS: [AttrInfo; 6] = [
    A_STAGE.attr_info(),
    A_PAIRED_COUNT.attr_info(),
    A_EXPOSED_COUNT.attr_info(),
    A_RESTART_REQUIRED.attr_info(),
    A_RX_REARM_COUNT.attr_info(),
    A_TX_DROPPED.attr_info(),
];
pub static DIRECTION_ATTRS: [AttrInfo; 1] = [A_DIRECTION.attr_info()];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Off,
    Low,
    Medium,
    High,
    /// ZCL `on` (4): the remote's fourth speed.
    On,
}

/// ZCL `FanModeSequence` (0x0001): the cycle a coordinator should present.
///
/// The fan has four speeds, and only three of them are standard modes: the
/// fourth is ZCL `on`, which no sequence value covers. Claiming
/// `Low/Med/High/Auto` (the ZCL default) would advertise an `auto` mode this
/// fan does not have, so the closest true statement is `Low/Med/High` - `on` is
/// still accepted as a mode, it is just not part of the advertised cycle.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FanModeSeq {
    LowMedHigh,
}

impl ZclEnum8 for FanModeSeq {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(FanModeSeq::LowMedHigh),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            FanModeSeq::LowMedHigh => 0x00,
        }
    }
}

impl ZclEnum8 for FanMode {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(FanMode::Off),
            1 => Some(FanMode::Low),
            2 => Some(FanMode::Medium),
            3 => Some(FanMode::High),
            4 => Some(FanMode::On),
            _ => None,
        }
    }
    fn into_raw(self) -> u8 {
        match self {
            FanMode::Off => 0,
            FanMode::Low => 1,
            FanMode::Medium => 2,
            FanMode::High => 3,
            FanMode::On => 4,
        }
    }
}

pub fn speed_of_mode(m: FanMode) -> u8 {
    match m {
        FanMode::Off => 0,
        FanMode::Low => 1,
        FanMode::Medium => 2,
        FanMode::High => 3,
        FanMode::On => 4,
    }
}

pub fn mode_of_speed(speed: u8) -> FanMode {
    match speed {
        1 => FanMode::Low,
        2 => FanMode::Medium,
        3 => FanMode::High,
        4 => FanMode::On,
        _ => FanMode::Off,
    }
}

pub fn ok(r: byte::Result<()>) -> Status {
    match r {
        Ok(()) => Status::Success,
        Err(_) => Status::InsufficientSpace,
    }
}

/// On/Off server: which slot it drives and what the two states mean.
pub struct OnOffEp<const EP: u8, const SLOT: i8, const KIND: u8>; // KIND 0=power 1=pairing

impl<const EP: u8, const SLOT: i8, const KIND: u8> OnOffEp<EP, SLOT, KIND> {
    fn get(&self) -> bool {
        STATE.lock(|s| {
            let st = s.borrow();
            match KIND {
                0 => st.cfg.slots[SLOT as usize].power,
                _ => st.pairing,
            }
        })
    }
    fn set(&self, on: bool) {
        match KIND {
            1 => {
                STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    st.pairing = on;
                    st.pair_step = 0;
                    st.pair_addr = 0;
                });
            }
            _ => {
                let code = STATE.lock(|s| {
                    let mut st = s.borrow_mut();
                    let sl = &mut st.cfg.slots[SLOT as usize];
                    if !sl.used() {
                        return 0;
                    }
                    sl.power = on;
                    let snap = st.cfg;
                    drop(st);
                    save_config(&snap);
                    let sl = &snap.slots[SLOT as usize];
                    let btn = if on {
                        speed_button(if sl.speed == 0 { 1 } else { sl.speed }).unwrap_or(BTN_SPEED1)
                    } else {
                        BTN_POWER
                    };
                    code_for(sl, btn)
                });
                if code != 0 {
                    tx_push_for(SLOT.max(0) as usize, code);
                }
            }
        }
        match KIND {
            0 => {
                zb_event(1, SLOT.max(0) as u8, u8::from(on));
                mark_fan_mode(SLOT.max(0) as usize);
            }
            _ => {
                zb_event(3, 0xFF, u8::from(on));
                mark_pairing_switch();
            }
        }
    }
}

impl<const EP: u8, const SLOT: i8, const KIND: u8> ClusterServer for OnOffEp<EP, SLOT, KIND> {
    fn cluster(&self) -> Cluster {
        CL_ONOFF
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &ONOFF_ATTRS
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        ok(A_ONOFF.encode(self.get(), out, offset))
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn command(&self, c: ClusterCommand<'_>, _out: &mut [u8]) -> CommandOutcome {
        match c.id.0 {
            0x00 => {
                self.set(false);
                CommandOutcome::Status(Status::Success)
            }
            0x01 => {
                self.set(true);
                CommandOutcome::Status(Status::Success)
            }
            0x02 => {
                self.set(!self.get());
                CommandOutcome::Status(Status::Success)
            }
            _ => CommandOutcome::Status(Status::UnsupCommand),
        }
    }
}

impl<const EP: u8, const SLOT: i8, const KIND: u8> ClusterRequestHandler
    for OnOffEp<EP, SLOT, KIND>
{
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP {
            return None;
        }
        self.handle_request(r, out)
    }
}

/// Speed server on the ZCL Fan Control cluster (hvacFanCtrl), which Z2M turns
/// into a `fan` entity (with mode) per endpoint.
pub struct FanEp<const SLOT: usize>;

impl<const SLOT: usize> ClusterServer for FanEp<SLOT> {
    fn cluster(&self) -> Cluster {
        CL_FAN
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &FAN_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        match id.0 {
            0x0001 => return ok(A_FAN_MODE_SEQ.encode(FanModeSeq::LowMedHigh, out, offset)),
            0x0000 => {}
            _ => return Status::UnsupportedAttribute,
        }
        let mode = STATE.lock(|s| {
            let st = s.borrow();
            let slot = &st.cfg.slots[SLOT];
            if slot.power {
                mode_of_speed(slot.speed)
            } else {
                FanMode::Off
            }
        });
        ok(A_FANMODE.encode(mode, out, offset))
    }
    fn decode_value(
        &self,
        id: AttributeId,
        ty: TypeId,
        bytes: &[u8],
        offset: &mut usize,
    ) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let Ok(mode) = A_FANMODE.decode(ty, bytes, offset) else {
            return Status::InvalidDataType;
        };
        self.apply(speed_of_mode(mode));
        Status::Success
    }
}

impl<const SLOT: usize> FanEp<SLOT> {
    fn apply(&self, speed: u8) {
        let code = STATE.lock(|s| {
            let mut st = s.borrow_mut();
            let sl = &mut st.cfg.slots[SLOT];
            if !sl.used() {
                return 0;
            }
            if speed == 0 {
                if !sl.power {
                    return 0;
                }
                sl.power = false;
                let snap = st.cfg;
                drop(st);
                save_config(&snap);
                return code_for(&snap.slots[SLOT], BTN_POWER);
            }
            sl.speed = speed;
            sl.power = true;
            let snap = st.cfg;
            drop(st);
            save_config(&snap);
            code_for(&snap.slots[SLOT], speed_button(speed).unwrap())
        });
        if code != 0 {
            tx_push_for(SLOT, code);
        }
        zb_event(0, SLOT as u8, speed);
        mark_fan_mode(SLOT);
    }
}

/// Status server for the pairing state machine, on the pairing endpoint.
pub struct PairingEp;

impl ClusterServer for PairingEp {
    fn cluster(&self) -> Cluster {
        CL_PAIRING
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &PAIRING_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        let (count, exposed, restart) = STATE.lock(|s| {
            let st = s.borrow();
            (st.paired_count(), st.exposed_count(), st.restart_required())
        });
        let stage = Stage::from_raw(STAGE_NOW.load(Ordering::Relaxed) as u8).unwrap_or(Stage::Idle);
        ok(match id.0 {
            0x0000 => A_STAGE.encode(stage, out, offset),
            0x0001 => A_PAIRED_COUNT.encode(Uint8(count), out, offset),
            0x0002 => A_EXPOSED_COUNT.encode(Uint8(exposed), out, offset),
            0x0003 => A_RESTART_REQUIRED.encode(restart, out, offset),
            0x0004 => A_RX_REARM_COUNT.encode(
                Uint16(REARM_COUNT.load(Ordering::Relaxed) as u16),
                out,
                offset,
            ),
            0x0005 => A_TX_DROPPED.encode(Uint16(tx_dropped() as u16), out, offset),
            _ => return Status::UnsupportedAttribute,
        })
    }

    /// `reboot` (0x00) applies a pending pairing change: the endpoint set is
    /// built at boot, so a new or deleted remote only becomes visible after a
    /// restart. Handing this to the user as a button beats "now go and reboot it
    /// from the console".
    fn command(&self, cmd: ClusterCommand<'_>, _out: &mut [u8]) -> CommandOutcome {
        match cmd.id.0 {
            0x00 => {
                // late enough that this response is on the air first
                request_reboot_in(300);
                CommandOutcome::Status(Status::Success)
            }
            _ => CommandOutcome::Status(Status::UnsupCommand),
        }
    }
}

impl ClusterRequestHandler for PairingEp {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_MASTER {
            return None;
        }
        self.handle_request(r, out)
    }
}

/// The stage last published to the coordinator, as a raw enum value; a read
/// sees the same value a report carries.
pub static STAGE_NOW: AtomicU32 = AtomicU32::new(0);

/// Direction server, on the same endpoint as the fan it belongs to: ZCL has no
/// three-state direction attribute, so this is a manufacturer-specific cluster.
pub struct DirEp<const SLOT: usize>;

impl<const SLOT: usize> ClusterServer for DirEp<SLOT> {
    fn cluster(&self) -> Cluster {
        CL_DIRECTION
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &DIRECTION_ATTRS
    }
    fn reporting(&self) -> Option<&dyn AttributeReporting> {
        Some(&REPORTING)
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let dir = STATE.lock(|s| Direction::from_u8(s.borrow().cfg.slots[SLOT].dir));
        ok(A_DIRECTION.encode(dir, out, offset))
    }
    fn decode_value(
        &self,
        id: AttributeId,
        ty: TypeId,
        bytes: &[u8],
        offset: &mut usize,
    ) -> Status {
        if id.0 != 0x0000 {
            return Status::UnsupportedAttribute;
        }
        let Ok(dir) = A_DIRECTION.decode(ty, bytes, offset) else {
            return Status::InvalidDataType;
        };
        self.apply(dir);
        Status::Success
    }
}

impl<const SLOT: usize> DirEp<SLOT> {
    fn apply(&self, dir: Direction) {
        let code = STATE.lock(|s| {
            let mut st = s.borrow_mut();
            let slot = &mut st.cfg.slots[SLOT];
            if !slot.used() {
                return 0;
            }
            slot.dir = dir.as_u8();
            let snap = st.cfg;
            drop(st);
            save_config(&snap);
            code_for(&snap.slots[SLOT], dir_button(dir.as_u8()).unwrap())
        });
        if code != 0 {
            tx_push_for(SLOT, code);
        }
        zb_event(2, SLOT as u8, dir.as_u8());
        mark_direction(SLOT);
    }
}

impl<const SLOT: usize> ClusterRequestHandler for DirEp<SLOT> {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_FAN0 + SLOT as u8 {
            return None;
        }
        self.handle_request(r, out)
    }
}

impl<const SLOT: usize> ClusterRequestHandler for FanEp<SLOT> {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != EP_FAN0 + SLOT as u8 {
            return None;
        }
        self.handle_request(r, out)
    }
}

// ---- identity
/// What this build is. Printed on boot and reported as `SWBuildID`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FANBRIDGE_GIT"));
/// Build date, `YYYYMMDD`, reported as `DateCode`.
pub const BUILD_DATE: &str = env!("FANBRIDGE_DATE");
/// What the Basic cluster reports as `SWBuildID`; ZCL limits it to 16 bytes,
/// so it is the tag when it fits and the commit otherwise.
pub const SW_BUILD_ID: &str = env!("FANBRIDGE_SW_BUILD_ID");

pub const VENDOR: &str = "locals.tf";
pub const MODEL_MASTER: &str = "esp-fanbridge";

/// `DateCode` (ZCL 3.2.2.2.8) and `SWBuildID` (ZCL 3.2.2.2.12) are not in the
/// minimal set the stack's `BasicServer` exposes, but they are what consumers
/// show as the firmware/software version.
pub const A_DATE_CODE: Attribute<ShortText, ReadOnly> =
    basic::CLUSTER.attribute(AttributeId(0x0006), "DateCode");
pub const A_SW_BUILD_ID: Attribute<ShortText, ReadOnly> =
    basic::CLUSTER.attribute(AttributeId(0x4000), "SWBuildID");

pub static BASIC_ATTRS: [AttrInfo; 9] = [
    basic::ZCL_VERSION.attr_info(),
    basic::APPLICATION_VERSION.attr_info(),
    basic::STACK_VERSION.attr_info(),
    basic::HW_VERSION.attr_info(),
    basic::MANUFACTURER_NAME.attr_info(),
    basic::MODEL_IDENTIFIER.attr_info(),
    A_DATE_CODE.attr_info(),
    basic::POWER_SOURCE.attr_info(),
    A_SW_BUILD_ID.attr_info(),
];

/// `Basic` server with a build identity on top of the mandatory attributes.
pub struct Basic {
    /// Endpoint this identity belongs to.
    pub ep: u8,
    pub manufacturer: &'static str,
    pub model: &'static str,
}

/// Encode a `CharacterString` attribute, or bail out if it does not fit.
macro_rules! text_attr {
    ($attr:expr, $text:expr, $out:expr, $off:expr) => {{
        let Some(value) = ShortStr::new($text) else {
            return Status::InvalidValue;
        };
        $attr.encode(value, $out, $off)
    }};
}

impl ClusterRequestHandler for Basic {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        if r.dst_endpoint != self.ep {
            return None;
        }
        self.handle_request(r, out)
    }
}

impl ClusterServer for Basic {
    fn cluster(&self) -> Cluster {
        basic::CLUSTER
    }
    fn attributes(&self) -> &'static [AttrInfo] {
        &BASIC_ATTRS
    }
    fn encode_value(&self, id: AttributeId, out: &mut [u8], offset: &mut usize) -> Status {
        ok(match id.0 {
            0x0000 => basic::ZCL_VERSION.encode(Uint8(8), out, offset),
            0x0001 => basic::APPLICATION_VERSION.encode(Uint8(1), out, offset),
            0x0002 => basic::STACK_VERSION.encode(Uint8(0), out, offset),
            0x0003 => basic::HW_VERSION.encode(Uint8(1), out, offset),
            0x0004 => text_attr!(basic::MANUFACTURER_NAME, self.manufacturer, out, offset),
            0x0005 => text_attr!(basic::MODEL_IDENTIFIER, self.model, out, offset),
            0x0006 => text_attr!(A_DATE_CODE, BUILD_DATE, out, offset),
            0x0007 => basic::POWER_SOURCE.encode(basic::PowerSource(0x01), out, offset),
            0x4000 => text_attr!(A_SW_BUILD_ID, SW_BUILD_ID, out, offset),
            _ => return Status::UnsupportedAttribute,
        })
    }
}

pub static BASIC_MASTER: Basic = Basic {
    ep: EP_MASTER,
    manufacturer: VENDOR,
    model: MODEL_MASTER,
};
pub static BASIC_F0: Basic = Basic {
    ep: EP_FAN0,
    manufacturer: VENDOR,
    model: "fan-1",
};
pub static BASIC_F1: Basic = Basic {
    ep: EP_FAN0 + 1,
    manufacturer: VENDOR,
    model: "fan-2",
};
pub static BASIC_F2: Basic = Basic {
    ep: EP_FAN0 + 2,
    manufacturer: VENDOR,
    model: "fan-3",
};
pub static BASIC_F3: Basic = Basic {
    ep: EP_FAN0 + 3,
    manufacturer: VENDOR,
    model: "fan-4",
};
pub static BASIC_F4: Basic = Basic {
    ep: EP_FAN0 + 4,
    manufacturer: VENDOR,
    model: "fan-5",
};
pub static BASIC_F5: Basic = Basic {
    ep: EP_FAN0 + 5,
    manufacturer: VENDOR,
    model: "fan-6",
};
pub static BASIC_F6: Basic = Basic {
    ep: EP_FAN0 + 6,
    manufacturer: VENDOR,
    model: "fan-7",
};
pub static BASIC_F7: Basic = Basic {
    ep: EP_FAN0 + 7,
    manufacturer: VENDOR,
    model: "fan-8",
};

pub static IDENTIFY: IdentifyServer = IdentifyServer::new();
