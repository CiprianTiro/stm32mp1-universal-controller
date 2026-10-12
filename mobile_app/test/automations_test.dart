import 'package:flutter_test/flutter_test.dart';
import 'package:house_controller_app/automations/editors.dart';
import 'package:house_controller_app/automations/model.dart';
import 'package:house_controller_app/hub/device.dart';
import 'package:house_controller_app/hub/hub_store.dart';

void main() {
  final hub = HubStore();

  test('triggers read like the wiki examples', () {
    expect(describeTrigger(hub, {'type': 'time', 'at': '07:30', 'days': ['mon', 'tue', 'wed', 'thu', 'fri']}), 'At 07:30 · weekdays');
    expect(describeTrigger(hub, {'type': 'sun', 'event': 'sunset', 'offset_min': -15}), 'At sunset −15 min');
    expect(describeTrigger(hub, {'type': 'threshold', 'device': 'plug', 'capability': 'energy', 'field': 'power_w', 'above': 5}),
        'When plug (removed): energy.power_w goes above 5');
  });

  test('conditions, across midnight and with one side open', () {
    expect(describeCondition(hub, {'type': 'time', 'after': '22:00', 'before': '06:00'}), 'Between 22:00 and 06:00');
    expect(describeCondition(hub, {'type': 'time', 'after': '18:00'}), 'After 18:00');
    expect(describeCondition(hub, {'type': 'sun', 'is': 'night'}), 'Only at night');
  });

  test('an unlock step says so loudly', () {
    expect(describeStep(hub, {'device': 'door', 'capability': 'lock', 'value': {'state': 'unlocked', 'confirmed': true}}),
        'door (removed): lock → UNLOCK');
  });

  test('watchable fields are paths into the state, without what the device only describes', () {
    final d = Device.fromJson({
      'id': 'x',
      'name': 'X',
      'capabilities': {
        'switch': {'on': true},
        'sensor': {'readings': {'temperature': {'value': 21.5, 'unit': '°C'}}},
        'cover': {'position': 40, 'moving': 'stopped', 'can_position': true},
        'media': {'volume': 3, 'muted': false, 'inputs': [{'id': 'HDMI_1', 'label': 'PS'}]},
      },
    });
    final f = watchableFields(d);
    expect(f['switch'], {'on': true});
    expect(f['sensor']!['readings.temperature.value'], 21.5);
    expect(f['cover']!.containsKey('can_position'), isFalse);
    expect(f['media']!.containsKey('inputs'), isFalse);
  });

  test('a new command starts from the state, without device-only parts', () {
    final d = Device.fromJson({
      'id': 'ac',
      'name': 'AC',
      'capabilities': {
        'climate': {'mode': 'cool', 'target': 23, 'current': 25.5, 'modes': ['off', 'cool'], 'fan': 'auto', 'fans': ['auto']},
        'media': {'volume': 7, 'muted': true, 'input': 'HDMI_1', 'inputs': []},
      },
    });
    expect(defaultValue(d, 'climate'), {'mode': 'cool', 'target': 23, 'fan': 'auto'});
    expect(defaultValue(d, 'media'), {'volume': 7, 'muted': true, 'input': 'HDMI_1'});
  });

  test('typed values: JSON when it is JSON', () {
    expect(parseLoose('true'), true);
    expect(parseLoose(' 21.5 '), 21.5);
    expect(parseLoose('locked'), 'locked');
  });
}
