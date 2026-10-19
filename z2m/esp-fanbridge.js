// External converter for the esp-fanbridge firmware.
//
// The device support comes from this file instead of Zigbee2MQTT's generated
// definition because the pairing status lives on a manufacturer-specific
// cluster, which a generated definition can not expose.
//
// Install: copy this file into Zigbee2MQTT's data directory and add
//   external_converters: [esp-fanbridge.js]
// to configuration.yaml, then restart Zigbee2MQTT.

const fz = require('zigbee-herdsman-converters/converters/fromZigbee');
const tz = require('zigbee-herdsman-converters/converters/toZigbee');
const exposes = require('zigbee-herdsman-converters/lib/exposes');
const constants = require('zigbee-herdsman-converters/lib/constants');
const utils = require('zigbee-herdsman-converters/lib/utils');

const ea = exposes.access;

const PAIRING_STAGES = [
    'idle',
    'waiting_speed_1',
    'waiting_speed_2',
    'waiting_speed_3',
    'waiting_speed_4',
    'paired',
    'failed',
];

// Manufacturer-specific cluster the firmware serves on the pairing endpoint.
const PAIRING_CLUSTER = {
    name: 'espPairing',
    ID: 0xfc00,
    attributes: {
        stage: {name: 'stage', ID: 0x0000, type: 0x30, write: false},
        pairedCount: {name: 'pairedCount', ID: 0x0001, type: 0x20, write: false},
    },
    commands: {},
    commandsResponse: {},
};

// `exposes` is also called with a placeholder device (without endpoints) while
// Zigbee2MQTT validates a definition, so the accessors have to tolerate that.
const endpoints = (device) => (device && Array.isArray(device.endpoints) ? device.endpoints : []);

/** Endpoints that carry a fan (ZCL Fan Control). */
const fanEndpoints = (device) => endpoints(device).filter((endpoint) => endpoint.supportsInputCluster('hvacFanCtrl')).map((endpoint) => endpoint.ID);

/** Endpoints that carry an on/off cluster (pairing switch, direction switches). */
const onOffEndpoints = (device) => endpoints(device).filter((endpoint) => endpoint.supportsInputCluster('genOnOff')).map((endpoint) => endpoint.ID);

module.exports = {
    zigbeeModel: ['esp-fanbridge'],
    model: 'esp-fanbridge',
    vendor: 'locals.tf',
    description: '433 MHz ceiling fan remote bridge',
    meta: {multiEndpoint: true, multiEndpointSkip: ['linkquality']},
    // every endpoint stands for itself; this is what suffixes state/fan_mode
    endpoint: (device) => Object.fromEntries(endpoints(device).map((endpoint) => [endpoint.ID, endpoint.ID])),
    exposes: (device) => {
        const result = [
            new exposes.Enum('pairing_stage', ea.STATE, PAIRING_STAGES).withDescription('Pairing stage'),
            new exposes.Numeric('paired_count', ea.STATE).withDescription('Number of paired remotes'),
        ];
        for (const endpoint of onOffEndpoints(device)) {
            result.push(new exposes.Switch().withState('state').withEndpoint(String(endpoint)));
        }
        for (const endpoint of fanEndpoints(device)) {
            result.push(exposes.presets.fan().withState('state').withModes(Object.keys(constants.fanMode)).withEndpoint(String(endpoint)));
        }
        return result;
    },
    fromZigbee: [
        {
            cluster: 'hvacFanCtrl',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.data.fanMode === undefined) return;
                const mode = utils.getKey(constants.fanMode, msg.data.fanMode);
                return {
                    [utils.postfixWithEndpointName('fan_mode', msg, model, meta)]: mode,
                    [utils.postfixWithEndpointName('state', msg, model, meta)]: mode === 'off' ? 'OFF' : 'ON',
                };
            },
        },
        fz.on_off,
        {
            cluster: 'espPairing',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg) => {
                const result = {};
                if (msg.data.stage !== undefined) result.pairing_stage = PAIRING_STAGES[msg.data.stage];
                if (msg.data.pairedCount !== undefined) result.paired_count = msg.data.pairedCount;
                return result;
            },
        },
    ],
    toZigbee: [tz.fan_mode, tz.on_off],
    onEvent: [
        async (event) => {
            // Register as early as possible: the cluster has to be known before
            // the first report from it can be parsed into named attributes.
            if (event.type === 'start' || event.type === 'deviceInterview' || event.type === 'deviceJoined') {
                event.data.device.addCustomCluster('espPairing', PAIRING_CLUSTER);
            }
        },
    ],
    configure: async (device) => {
        device.addCustomCluster('espPairing', PAIRING_CLUSTER);
        // The firmware reports on change to the coordinator, so no binding is
        // needed; ask for the pairing stage so the value starts out known.
        const endpoint = device.endpoints.find((e) => e.supportsInputCluster('espPairing'));
        if (endpoint) {
            await endpoint.read('espPairing', ['stage', 'pairedCount']).catch(() => {});
        }
    },
};
