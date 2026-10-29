// Tests for the external converter's payload mapping.
//
// The converter is where the two things that broke in practice live: which
// cluster a `state` write goes to, and what a stopped fan reports about its
// speed. Neither needs a device to check - the functions are pure given an
// endpoint and a message - so they are checked here instead of by hand in
// Home Assistant.
//
//   cd z2m && npm install && node test.js
//
// `zigbee-herdsman-converters` is the only dependency, and the version is pinned
// in package.json to the one the firmware was verified against.

const assert = require('node:assert/strict');

const converter = process.env.CONVERTER || './esp-fanbridge';
const definition = require(converter);

let failed = 0;

async function test(name, fn) {
    try {
        await fn();
        console.log(`ok   ${name}`);
    } catch (e) {
        failed += 1;
        console.log(`FAIL ${name}`);
        console.log(`     ${e.message.split('\n').join('\n     ')}`);
    }
}

/** A fake coordinator endpoint that records what the converter does to it. */
function endpoint(ID, clusters, failures = 0) {
    const calls = [];
    const state = {failures, values: {}};
    return {
        ID,
        calls,
        failNextWrites: (n) => {
            state.failures = n;
        },
        supportsInputCluster: (key) => clusters.includes(key),
        supportsOutputCluster: () => false,
        write: async (cluster, payload, options) => {
            calls.push({op: 'write', cluster, payload, options});
            if (state.failures > 0) {
                state.failures -= 1;
                throw new Error('MAC_TRANSACTION_EXPIRED');
            }
        },
        read: async (cluster, attrs) => {
            calls.push({op: 'read', cluster, attrs});
            if (state.failures > 0) {
                state.failures -= 1;
                throw new Error('MAC_TRANSACTION_EXPIRED');
            }
            // a read-back returns whatever the device currently holds
            for (const attribute of attrs) {
                if (state.values[`${cluster}.${attribute}`] === undefined) {
                    state.values[`${cluster}.${attribute}`] = state.pending?.[`${cluster}.${attribute}`];
                }
            }
        },
        hold: (cluster, attribute, value) => {
            state.values[`${cluster}.${attribute}`] = value;
        },
        command: async (cluster, cmd) => calls.push({op: 'command', cluster, cmd}),
        getClusterAttributeValue: (cluster, attribute) => state.values[`${cluster}.${attribute}`],
    };
}

const FAN_CLUSTERS = ['genBasic', 'hvacFanCtrl', 'espDirection'];
const SWITCH_CLUSTERS = ['genBasic', 'genOnOff'];
const fanEndpoint = (id) => endpoint(id, FAN_CLUSTERS);
const switchEndpoint = (id) => endpoint(id, SWITCH_CLUSTERS);

/** The context Zigbee2MQTT would pass in, with just what the converter uses. */
const device = {
    endpoints: [switchEndpoint(1), fanEndpoint(2), fanEndpoint(3), fanEndpoint(4)],
};
// `message` is the original set payload: the stock on/off converter reads it
const context = () => ({device, mapped: definition, state: {}, message: {}});

const fromZigbee = (cluster) => definition.fromZigbee.find((c) => c.cluster === cluster);
const toZigbee = (key) => definition.toZigbee.find((c) => c.key.includes(key));

const fanModeConvert = (data, ID) =>
    fromZigbee('hvacFanCtrl').convert({meta: {multiEndpoint: true}}, {data, endpoint: {ID}}, () => {}, {}, context());

const directionConvert = (data, ID) =>
    fromZigbee('espDirection').convert({meta: {multiEndpoint: true}}, {data, endpoint: {ID}}, () => {}, {}, context());

(async () => {
    // ---- what the device reports

    await test('a stopped fan reports off without forgetting its speed', async () => {
        // backlog #41: `off` is a state, not a speed of zero
        assert.deepEqual(fanModeConvert({fanMode: 0}, 3), {state_3: 'OFF'});
    });

    await test('low/medium/high map to speeds 1/2/3', async () => {
        assert.deepEqual(fanModeConvert({fanMode: 1}, 2), {state_2: 'ON', speed_2: 1});
        assert.deepEqual(fanModeConvert({fanMode: 2}, 2), {state_2: 'ON', speed_2: 2});
        assert.deepEqual(fanModeConvert({fanMode: 3}, 2), {state_2: 'ON', speed_2: 3});
    });

    await test('the fourth speed is ZCL `on`, not `high`', async () => {
        // mapping it to `high` would collapse two speeds into one in HA
        assert.deepEqual(fanModeConvert({fanMode: 4}, 4), {state_4: 'ON', speed_4: 4});
    });

    await test('directions map in/out/auto', async () => {
        assert.deepEqual(directionConvert({direction: 0}, 2), {direction_2: 'in'});
        assert.deepEqual(directionConvert({direction: 1}, 2), {direction_2: 'out'});
        assert.deepEqual(directionConvert({direction: 2}, 2), {direction_2: 'auto'});
    });

    await test('an unknown direction value falls back to in', async () => {
        assert.deepEqual(directionConvert({direction: 9}, 2), {direction_2: 'in'});
    });

    await test('the pairing payload derives restart_required from the counts', async () => {
        const convert = fromZigbee('espPairing').convert;
        assert.deepEqual(convert(definition, {data: {stage: 5, pairedCount: 3, exposedCount: 3}}), {
            pairing_stage: 'paired',
            paired_count: 3,
            exposed_count: 3,
            restart_required: false,
        });
        assert.deepEqual(convert(definition, {data: {stage: 0, pairedCount: 4, exposedCount: 3}}), {
            pairing_stage: 'idle',
            paired_count: 4,
            exposed_count: 3,
            restart_required: true,
        });
    });

    await test('the pairing payload carries the dropped-command counter', async () => {
        const convert = fromZigbee('espPairing').convert;
        assert.deepEqual(convert(definition, {data: {txDropped: 3}}), {tx_dropped: 3});
        // 0 must survive: "no drops" is the healthy value, not a missing one
        assert.deepEqual(convert(definition, {data: {txDropped: 0}}), {tx_dropped: 0});
        // a report that carries neither count must not invent restart_required
        assert.deepEqual(convert(definition, {data: {txDropped: 7}}), {tx_dropped: 7});
    });

    await test('a report with no known attribute produces nothing', async () => {
        assert.equal(fanModeConvert({}, 2), undefined);
    });

    // ---- what the coordinator asks for

    await test('state OFF on a fan stops it and keeps the speed', async () => {
        const entity = fanEndpoint(3);
        const meta = context();
        meta.state = {speed_3: 4};
        const result = await toZigbee('state').convertSet(entity, 'state', 'OFF', meta);
        assert.equal(entity.calls.length, 1);
        assert.equal(entity.calls[0].cluster, 'hvacFanCtrl');
        assert.deepEqual(entity.calls[0].payload, {fanMode: 0});
        assert.deepEqual(result, {state: {state: 'OFF'}});
    });

    await test('state ON on a fan restores the remembered speed', async () => {
        const entity = fanEndpoint(3);
        const meta = context();
        meta.state = {speed_3: 3};
        const result = await toZigbee('state').convertSet(entity, 'state', 'ON', meta);
        assert.equal(entity.calls.length, 1);
        assert.equal(entity.calls[0].cluster, 'hvacFanCtrl');
        assert.deepEqual(entity.calls[0].payload, {fanMode: 3});
        assert.deepEqual(result, {state: {state: 'ON', speed: 3}});
    });

    await test('state ON on a switch is an OnOff command, not FanMode', async () => {
        // this is what made the pairing switch dead: the fan converter claimed
        // `state` and wrote FanMode to an endpoint without a fan controller
        const entity = switchEndpoint(1);
        const meta = context();
        meta.message = {state: 'ON'};
        await toZigbee('state').convertSet(entity, 'state', 'ON', meta);
        assert.deepEqual(entity.calls, [{op: 'command', cluster: 'genOnOff', cmd: 'on'}]);
    });

    await test('a lost reply is confirmed by reading the value back', async () => {
        // the device applied it; only the reply was lost, so the read-back says so
        const entity = fanEndpoint(2);
        entity.hold('espDirection', 'direction', 1); // the device already holds it
        entity.failNextWrites(1);
        await toZigbee('direction').convertSet(entity, 'direction', 'out', context());
        const ops = entity.calls.map((c) => c.op);
        assert.deepEqual(ops, ['write', 'read']);
    });

    await test('a write that did not land is reported', async () => {
        const entity = fanEndpoint(2);
        entity.hold('espDirection', 'direction', 0); // device still says "in"
        entity.failNextWrites(1);
        await assert.rejects(
            toZigbee('direction').convertSet(entity, 'direction', 'out', context()),
            /expected 1/,
        );
    });

    await test('a lost reply with a failed read-back is assumed applied', async () => {
        // the write reaches the device; only replies go missing, and a throw
        // would make Zigbee2MQTT undo a command that worked
        const entity = fanEndpoint(2);
        entity.failNextWrites(2); // write and read both fail
        await toZigbee('direction').convertSet(entity, 'direction', 'out', context());
    });

    await test('a speed outside 1..4 is clamped', async () => {
        for (const [given, want] of [[9, 4], [0, 1], [2, 2]]) {
            const entity = fanEndpoint(2);
            await toZigbee('speed').convertSet(entity, 'speed', given, context());
            assert.equal(entity.calls[0].payload.fanMode, want, `speed ${given}`);
        }
    });

    await test('a direction write goes to the direction cluster', async () => {
        for (const [value, raw] of [['in', 0], ['out', 1], ['auto', 2]]) {
            const entity = fanEndpoint(2);
            await toZigbee('direction').convertSet(entity, 'direction', value, context());
            assert.equal(entity.calls.length, 1);
            assert.equal(entity.calls[0].cluster, 'espDirection');
            assert.deepEqual(entity.calls[0].payload, {direction: raw});
        }
    });

    await test('apply_pairing_change sends the reboot command', async () => {
        const entity = switchEndpoint(1);
        await toZigbee('apply_pairing_change').convertSet(entity, 'apply_pairing_change', 'apply', context());
        assert.deepEqual(entity.calls, [{op: 'command', cluster: 'espPairing', cmd: 'reboot'}]);
    });

    await test('an unknown action value does nothing', async () => {
        const entity = switchEndpoint(1);
        await toZigbee('apply_pairing_change').convertSet(entity, 'apply_pairing_change', 'nonsense', context());
        assert.deepEqual(entity.calls, []);
    });

    // ---- what Home Assistant sees

    await test('exposes follow the endpoints the device has', async () => {
        const exposes = definition.exposes(device);
        const fans = exposes.filter((e) => e.type === 'fan').map((e) => e.endpoint);
        const switches = exposes.filter((e) => e.type === 'switch').map((e) => e.endpoint);
        const directions = exposes.filter((e) => e.name === 'direction').map((e) => e.endpoint);
        assert.deepEqual(fans, ['2', '3', '4']);
        assert.deepEqual(switches, ['1']);
        assert.deepEqual(directions, ['2', '3', '4']);
    });

    await test('exposes tolerate a device without endpoints yet', async () => {
        // Zigbee2MQTT validates a definition with a placeholder device
        const exposes = definition.exposes({});
        assert.ok(exposes.some((e) => e.name === 'pairing_stage'));
        assert.equal(exposes.filter((e) => e.type === 'fan').length, 0);
    });

    console.log(failed === 0 ? '\nall converter tests passed' : `\n${failed} converter test(s) failed`);
    process.exit(failed === 0 ? 0 : 1);
})();
