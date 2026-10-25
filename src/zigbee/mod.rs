//! The Zigbee layer: what this device looks like on the network.
//!
//! `clusters` holds one server per ZCL cluster, `report` sends attribute
//! reports, and this module ties them together - the endpoint table built from
//! the configuration, the handler chain that dispatches by endpoint, the stack
//! configuration and the tasks that drive it.

mod clusters;
mod report;

pub use clusters::*;
pub use report::*;

use core::cell::RefCell;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::Mutex as CsMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Delay as AsyncDelay;
use embassy_time::Timer;
use esp_println::println;
use zigbee::CurrentPowerMode;
use zigbee::CurrentPowerSourceLevel;
use zigbee::DeviceConfig;
use zigbee::LogicalType;
use zigbee::NetworkConfig;
use zigbee::PowerSource;
use zigbee::TimingConfig;
use zigbee::config::StackConfig;
use zigbee::nwk::nib::CapabilityInformation;
use zigbee::zdo::ClusterReply;
use zigbee::zdo::ClusterRequest;
use zigbee::zdo::ClusterRequestHandler;
use zigbee::zdo::config::DiscoveryType;
use zigbee::zdo::descriptor::DeviceDescriptorConfig;
use zigbee::zdo::descriptor::EndpointDescriptor;
use zigbee::zdo::descriptor::NodeDescriptorConfig;
use zigbee::zdo::descriptor::PowerDescriptorConfig;
use zigbee_cluster_library::clusters::general::basic;
use zigbee_cluster_library::clusters::general::identify;
use zigbee_cluster_library::profile;
use zigbee_cluster_library::server::UnsupportedClusterResponder;
use zigbee_types::IeeeAddress;

use crate::config::Config;
use crate::state::PAIRING_REPORT;
use crate::state::REPORT_DIRTY;
use crate::state::REPORT_FAIL;
use crate::state::REPORT_OK;
use crate::state::mark_all_state;

// ==========================================================================
// Zigbee layer
// ==========================================================================

pub const NWK_EPID: u64 = 0xd928_6c81_6768_f40b;
pub const NWK_CHANNEL: u8 = 11;
pub const ZIGBEE_FLASH: core::ops::Range<u32> = 0x3f_0000..0x3f_4000;

pub const EP_MASTER: u8 = 1;
pub const EP_FAN0: u8 = 2; // ..9

// ---- endpoint table
pub static IN_MASTER: [u16; 3] = [basic::CLUSTER_ID, identify::CLUSTER_ID, 0x0006];
pub static IN_FAN: [u16; 3] = [basic::CLUSTER_ID, 0x0202, 0xFC01];
pub static NO_CLUSTERS: [u16; 0] = [];

/// Endpoint table built at boot from the config: only paired remotes get
/// endpoints, so a coordinator sees exactly the fans that exist.
pub static EP_BUF: static_cell::StaticCell<[EndpointDescriptor<'static>; 9]> =
    static_cell::StaticCell::new();

/// The slice handed to the stack, kept so `desc` can print it.
pub static EP_LIST: CsMutex<
    CriticalSectionRawMutex,
    RefCell<Option<&'static [EndpointDescriptor<'static>]>>,
> = CsMutex::new(RefCell::new(None));

pub fn master_descriptor() -> EndpointDescriptor<'static> {
    EndpointDescriptor {
        endpoint: EP_MASTER,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0x0000,
        device_version: 1,
        input_clusters: &IN_MASTER,
        output_clusters: &NO_CLUSTERS,
    }
}

pub fn fan_descriptor(slot: usize) -> EndpointDescriptor<'static> {
    EndpointDescriptor {
        endpoint: EP_FAN0 + slot as u8,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0x002B,
        device_version: 1,
        input_clusters: &IN_FAN,
        output_clusters: &NO_CLUSTERS,
    }
}

/// Build the advertised endpoint list: endpoint 1 always, then one endpoint per
/// paired slot (endpoints stay tied to their slot, so a removed slot leaves a
/// gap rather than renumbering the others).
pub fn build_endpoints(cfg: &Config) -> &'static [EndpointDescriptor<'static>] {
    let blank = EndpointDescriptor {
        endpoint: 0,
        profile_id: profile::HOME_AUTOMATION,
        device_id: 0,
        device_version: 1,
        input_clusters: &NO_CLUSTERS,
        output_clusters: &NO_CLUSTERS,
    };
    let buf = EP_BUF.init([blank; 9]);
    let mut n = 0;
    buf[n] = master_descriptor();
    n += 1;
    for (i, slot) in cfg.slots.iter().enumerate() {
        if slot.used() {
            buf[n] = fan_descriptor(i);
            n += 1;
        }
    }
    let list: &'static [EndpointDescriptor<'static>] = &buf[..n];
    EP_LIST.lock(|l| *l.borrow_mut() = Some(list));
    list
}

/// The advertised endpoints (empty before the table is built).
pub(crate) fn advertised() -> &'static [EndpointDescriptor<'static>] {
    EP_LIST.lock(|l| *l.borrow()).unwrap_or(&[])
}

pub static PAIR_SW: OnOffEp<1, -1, 1> = OnOffEp;
pub static PAIRING: PairingEp = PairingEp;
pub static PWR0: OnOffEp<2, 0, 0> = OnOffEp;
pub static PWR1: OnOffEp<3, 1, 0> = OnOffEp;
pub static PWR2: OnOffEp<4, 2, 0> = OnOffEp;
pub static PWR3: OnOffEp<5, 3, 0> = OnOffEp;
pub static PWR4: OnOffEp<6, 4, 0> = OnOffEp;
pub static PWR5: OnOffEp<7, 5, 0> = OnOffEp;
pub static PWR6: OnOffEp<8, 6, 0> = OnOffEp;
pub static PWR7: OnOffEp<9, 7, 0> = OnOffEp;
pub static FANDIR0: DirEp<0> = DirEp;
pub static FANDIR1: DirEp<1> = DirEp;
pub static FANDIR2: DirEp<2> = DirEp;
pub static FANDIR3: DirEp<3> = DirEp;
pub static FANDIR4: DirEp<4> = DirEp;
pub static FANDIR5: DirEp<5> = DirEp;
pub static FANDIR6: DirEp<6> = DirEp;
pub static FANDIR7: DirEp<7> = DirEp;
pub static FAN0: FanEp<0> = FanEp;
pub static FAN1: FanEp<1> = FanEp;
pub static FAN2: FanEp<2> = FanEp;
pub static FAN3: FanEp<3> = FanEp;
pub static FAN4: FanEp<4> = FanEp;
pub static FAN5: FanEp<5> = FanEp;
pub static FAN6: FanEp<6> = FanEp;
pub static FAN7: FanEp<7> = FanEp;

/// Answers `UNSUPPORTED_CLUSTER` to every request no server claimed.
pub static UNSUPPORTED: UnsupportedClusterResponder<'static> =
    UnsupportedClusterResponder::new(&[]);

/// Dispatches to all cluster servers in turn (trait objects avoid a 40-deep tuple).
pub struct Chain;

impl ClusterRequestHandler for Chain {
    fn handle(&self, r: &ClusterRequest<'_>, out: &mut [u8]) -> Option<ClusterReply> {
        const SRV: [&dyn ClusterRequestHandler; 37] = [
            &BASIC_MASTER,
            &IDENTIFY,
            &PAIR_SW,
            &PAIRING,
            &BASIC_F0,
            &FAN0,
            &PWR0,
            &FANDIR0,
            &BASIC_F1,
            &FAN1,
            &PWR1,
            &FANDIR1,
            &BASIC_F2,
            &FAN2,
            &PWR2,
            &FANDIR2,
            &BASIC_F3,
            &FAN3,
            &PWR3,
            &FANDIR3,
            &BASIC_F4,
            &FAN4,
            &PWR4,
            &FANDIR4,
            &BASIC_F5,
            &FAN5,
            &PWR5,
            &FANDIR5,
            &BASIC_F6,
            &FAN6,
            &PWR6,
            &FANDIR6,
            &BASIC_F7,
            &FAN7,
            &PWR7,
            &FANDIR7,
            // last: anything nobody handled still gets an answer, so a
            // coordinator does not wait for a response that never comes
            &UNSUPPORTED,
        ];
        SRV.iter().find_map(|h| h.handle(r, out))
    }
}

pub type ZbFlash = zigbee::storage::FlashStorage<
    embassy_embedded_hal::adapter::BlockingAsync<esp_storage::FlashStorage<'static>>,
>;
pub type ZbStack = zigbee::Stack<'static, zigbee_mac::esp::EspMlme<'static>, Chain, ZbFlash>;
pub static STACK: static_cell::StaticCell<ZbStack> = static_cell::StaticCell::new();

pub fn stack_config(endpoints: &'static [EndpointDescriptor<'static>]) -> StackConfig<'static> {
    StackConfig::new(
        NetworkConfig {
            extended_pan_id: IeeeAddress(NWK_EPID),
            channels: NWK_CHANNEL..NWK_CHANNEL + 1,
            scan_duration: 5,
        },
        DeviceConfig {
            logical_type: LogicalType::EndDevice,
            // bit7 allocate address, bit3 receiver-on-when-idle, bit2 mains
            // power. rx-on is not what this parent negotiates (it answers the
            // end-device timeout request with "keepalive by MAC data poll"), and
            // it does make the coordinator try direct transmissions that a
            // polling device misses - but a device that does NOT declare rx-on
            // cannot join at all on this stack: the network key is only fetched
            // by `poll_transport_key`, and that poll never yields it (see
            // backlog #46). Bootable first; see #45 for the structural fix.
            capability_information: CapabilityInformation(0x8c),
            discovery_type: DiscoveryType::default(),
            tc_link_key_exchange: true,
        },
        TimingConfig {
            poll_interval_ms: 100,
            ..TimingConfig::default()
        },
        DeviceDescriptorConfig {
            node: NodeDescriptorConfig {
                frequency_band: 0x08,
                manufacturer_code: 0x1037,
                maximum_buffer_size: 80,
                maximum_incoming_transfer_size: 128,
                server_mask: 0,
                maximum_outgoing_transfer_size: 128,
                descriptor_capability_field: 0,
                ..NodeDescriptorConfig::default()
            },
            power: PowerDescriptorConfig {
                current_power_mode: CurrentPowerMode::Stimulated,
                available_power_sources: &[PowerSource::ConstantMainPower],
                current_power_source: PowerSource::ConstantMainPower,
                current_power_source_level: CurrentPowerSourceLevel::Full,
            },
            endpoints,
        },
    )
}

/// Send reports from their own task.
///
/// A report waits for an APS acknowledgement; doing that inline in the main loop
/// would stall RF reception and the console whenever the coordinator is slow or
/// unreachable. Here a failed send just leaves the bit set for the next pass.
#[embassy_executor::task]
pub async fn report_task(stack: &'static ZbStack) {
    // A report that cannot be sent is retried, but not in a tight loop: before
    // the join completes every attempt fails instantly (SecurityFail), and with
    // a 50 ms cadence that is twenty failures a second flooding the console and
    // the radio exactly when the join needs them quiet.
    let mut backoff_ms: u64 = 50;
    loop {
        // Nothing can be reported before there is a network to report on. This
        // is a level check on the information base rather than
        // `wait_until_joined()`: that event has a single waker, and the receive
        // loop and the announce task wait on it too - a third waiter can miss
        // the wake-up and stall forever, which is exactly what happened when
        // this gate first used it (no reports at all after a boot).
        let addr = *zigbee::nwk::nib::get_ref().network_address();
        if addr == zigbee::nwk::nib::NWK_UNASSIGNED_ADDRESS {
            Timer::after_millis(250).await;
            continue;
        }

        let mut attempted = false;
        let mut sent = false;
        if PAIRING_REPORT.load(Ordering::Relaxed) {
            attempted = true;
            match send_pairing_report(stack).await {
                Ok(()) => {
                    PAIRING_REPORT.store(false, Ordering::Relaxed);
                    REPORT_OK.fetch_add(1, Ordering::Relaxed);
                    sent = true;
                }
                Err(e) => {
                    REPORT_FAIL.fetch_add(1, Ordering::Relaxed);
                    println!("rep fail: {} (pairing)", e.name());
                }
            }
        } else {
            let dirty = REPORT_DIRTY.load(Ordering::Relaxed);
            if dirty != 0 {
                attempted = true;
                let bit = dirty.trailing_zeros();
                match send_state_report(stack, bit).await {
                    Ok(()) => {
                        REPORT_DIRTY.fetch_and(!(1 << bit), Ordering::Relaxed);
                        REPORT_OK.fetch_add(1, Ordering::Relaxed);
                        sent = true;
                    }
                    Err(e) => {
                        REPORT_FAIL.fetch_add(1, Ordering::Relaxed);
                        println!("rep fail: {} (bit {bit})", e.name());
                    }
                }
            }
        }

        let wait = if attempted && !sent {
            backoff_ms = (backoff_ms * 2).min(3_000);
            backoff_ms
        } else {
            backoff_ms = 50;
            backoff_ms
        };
        Timer::after_millis(wait).await;
    }
}

/// Announce the device once it is on the network.
///
/// A resume after a reset keeps the stored short address but does not announce
/// it, so a coordinator that lost the route only rediscovers it after its writes
/// fail (Zigbee2MQTT logs `MAC_TRANSACTION_EXPIRED`, then does a `nwkAddrReq`).
/// Three quick broadcasts right after joining close that gap; they are cheap and
/// harmless when the coordinator already knows us.
#[embassy_executor::task]
pub async fn announce_task(stack: &'static ZbStack) {
    stack.wait_until_joined().await;
    // the coordinator starts from defaults: give it the real state once
    mark_all_state();
    for _ in 0..3 {
        match stack.device().announce_self().await {
            Ok(()) => println!("zigbee: announced"),
            Err(e) => println!("zigbee: announce failed: {e:?}"),
        }
        Timer::after_millis(400).await;
    }
}

#[embassy_executor::task]
pub async fn stack_task(stack: &'static ZbStack) {
    let outcome = stack.run(AsyncDelay).await;
    println!("zigbee stack stopped: {outcome:?}");
    esp_hal::system::software_reset();
}
