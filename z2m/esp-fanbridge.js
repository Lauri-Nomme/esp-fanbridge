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

/** ZCL fanMode names for speed 1..4 (the fourth speed is ZCL `on`). */
const SPEEDS = ['low', 'medium', 'high', 'on'];

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
            // Four discrete speeds (the remote's speed 1..4), exposed as the fan
            // percentage so Home Assistant offers four steps instead of three
            // modes plus an unrelated "on" preset.
            result.push(exposes.presets.fan().withState('state').withSpeed(1, 4).withEndpoint(String(endpoint)));
        }
        return result;
    },
    fromZigbee: [
        {
            // fanMode carries both the on/off state and the speed: `off` means
            // stopped, `low`..`on` are the remote's speed 1..4.
            cluster: 'hvacFanCtrl',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.data.fanMode === undefined) return;
                const mode = utils.getKey(constants.fanMode, msg.data.fanMode);
                const speed = SPEEDS.indexOf(mode) + 1; // 0 when not a speed
                return {
                    [utils.postfixWithEndpointName('state', msg, model, meta)]: speed === 0 ? 'OFF' : 'ON',
                    [utils.postfixWithEndpointName('speed', msg, model, meta)]: speed,
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
    toZigbee: [
        {
            key: ['state', 'speed'],
            convertSet: async (entity, key, value, meta) => {
                // Zigbee2MQTT adds the endpoint suffix itself, so return the
                // plain property names here.
                const speedProperty = utils.postfixWithEndpointName('speed', {endpoint: entity}, meta.mapped, meta);
                if (key === 'state' && value === 'OFF') {
                    await entity.write('hvacFanCtrl', {fanMode: constants.fanMode.off});
                    return {state: {state: 'OFF', speed: 0}};
                }
                // turning on keeps the speed the fan had, low if there was none
                const speed = key === 'speed' ? Math.min(Math.max(Number(value), 1), SPEEDS.length) : meta.state?.[speedProperty] || 1;
                await entity.write('hvacFanCtrl', {fanMode: constants.fanMode[SPEEDS[speed - 1]]});
                return {state: {state: 'ON', speed}};
            },
            convertGet: async (entity) => {
                if (entity.supportsInputCluster('hvacFanCtrl')) {
                    await entity.read('hvacFanCtrl', ['fanMode']);
                }
            },
        },
        tz.on_off,
    ],
    // Zigbee2MQTT calls `definition.onEvent` as a plain function; register the
    // cluster as early as possible so the first report can be parsed.
    onEvent: async (event) => {
        if (event.type === 'start' || event.type === 'deviceInterview' || event.type === 'deviceJoined') {
            event.data.device.addCustomCluster('espPairing', PAIRING_CLUSTER);
        }
    },
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
