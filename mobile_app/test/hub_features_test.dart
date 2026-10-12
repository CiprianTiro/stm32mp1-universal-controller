// What the hub gained after the app's first version (#74 vendor accounts,
// vacuums, cameras, device options; #99 "powered by"; #44 a TV's apps and
// channels): the app draws and steers them from the same JSON the hub's
// screen uses. A fake hub (HubStore.startDemo) records every request.
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:house_controller_app/hub/device.dart';
import 'package:house_controller_app/hub/hub_store.dart';
import 'package:house_controller_app/screens/wizard_screen.dart';
import 'package:house_controller_app/theme/tokens.dart';
import 'package:house_controller_app/widgets/capability_controls.dart';
import 'package:house_controller_app/widgets/power_link.dart';

/// A hub that answers like ws.rs for the few requests these screens send,
/// and remembers them. [devices]: what list_devices returns.
class FakeHub {
  FakeHub(this.devices);
  final List<Map<String, dynamic>> devices;
  final List<Map<String, dynamic>> sent = [];

  /// Answers to wizard requests, in order (wizard_start, wizard_answer...).
  final List<Map<String, dynamic>> wizardReplies = [];

  Future<Map<String, dynamic>> handle(Map<String, dynamic> m) async {
    sent.add(m);
    return switch (m['action']) {
      'list_devices' => {'type': 'devices', 'devices': devices},
      'get_settings' => {'type': 'settings'},
      'list_found' => {'type': 'found', 'devices': []},
      'list_automations' => {'type': 'automations', 'scenes': [], 'automations': []},
      'device_action' => {'type': 'action_result', 'result': {}},
      'set_power_link' => {'type': 'device', 'device': devices.first},
      final String a when a.startsWith('wizard_') => wizardReplies.removeAt(0),
      _ => {'type': 'ack'},
    };
  }

  List<Map<String, dynamic>> actions() => sent.where((m) => m['action'] == 'device_action').toList();
}

void main() {
  late DesignTokens tokens;
  setUpAll(() async {
    TestWidgetsFlutterBinding.ensureInitialized();
    tokens = await DesignTokens.load();
  });

  Future<HubStore> hubWith(FakeHub fake) async {
    final hub = HubStore();
    await hub.startDemo(fake.handle);
    return hub;
  }

  Widget app(Widget child) => MaterialApp(
        theme: tokens.theme(mode: 'dark', accent: 'sky', density: 'comfortable'),
        home: Scaffold(body: SingleChildScrollView(child: child)),
      );

  group('vacuum map (roborock_map.rs Map::to_json)', () {
    test('runs are unpacked exactly', () {
      final text = base64Encode([0, 3, 2, 1]);
      expect(VacuumMapData.unpackRuns(text, 4), [0, 0, 0, 2]);
      // Too few or too many squares: not this map.
      expect(VacuumMapData.unpackRuns(text, 5), isNull);
      expect(VacuumMapData.unpackRuns(text, 3), isNull);
      expect(VacuumMapData.unpackRuns('not base64!', 4), isNull);
    });

    test('a whole map, with dock, vacuum and path', () {
      final map = VacuumMapData.fromJson({
        'width': 4,
        'height': 3,
        'pixels': base64Encode([1, 4, 2, 4, 16, 4]),
        'dock': [1.0, 2.0],
        'robot': [2.5, 1.0, 90],
        'path': [[0, 0], [1, 1]],
        'no_go': [],
        'walls': [],
        'rooms': [6],
      })!;
      expect((map.width, map.height), (4, 3));
      expect(map.classes.sublist(8), [16, 16, 16, 16]);
      expect(map.dock, const Offset(1, 2));
      expect(map.robot, const Offset(2.5, 1));
      expect(map.path.length, 2);
      expect(VacuumMapData.fromJson({'width': 0, 'height': 3, 'pixels': ''}), isNull);
    });
  });

  testWidgets('a vacuum: state, modes, rooms, parts -- steered with its actions', (tester) async {
    final vacuum = {
      'id': 'robo',
      'name': 'Robo',
      'capabilities': {
        'vacuum': {
          'state': 'docked',
          'battery': 84,
          'fan': 'balanced',
          'fans': ['quiet', 'balanced', 'turbo'],
          'rooms': [
            {'id': 16, 'name': 'Kitchen'},
            {'id': 17, 'name': 'Hall'},
          ],
          'parts': [
            {'id': 'filter', 'name': 'Filter', 'left': 21},
          ],
          'totals': {'cleanings': 272, 'area_m2': 6977.2, 'hours': 117.7},
        },
      },
    };
    final fake = FakeHub([vacuum]);
    final hub = await hubWith(fake);
    await tester.pumpWidget(app(CapabilityControls(device: Device.fromJson(vacuum), hub: hub)));

    expect(find.text('On its dock'), findsOneWidget);
    expect(find.text(' 84 %'), findsOneWidget);
    expect(find.text('Filter'), findsOneWidget);
    expect(find.text('Update the app to control this'), findsNothing);

    await tester.tap(find.text('Start'));
    await tester.tap(find.text('Turbo'));
    await tester.tap(find.text('Kitchen'));
    await tester.pump();
    await tester.ensureVisible(find.text('Clean'));
    await tester.tap(find.text('Clean'));
    await tester.pump();
    expect(fake.actions().map((a) => [a['name'], a['args']]).toList(), [
      ['start', {}],
      ['set_fan', {'fan': 'turbo'}],
      ['clean_rooms', {'rooms': [16], 'repeat': 1}],
    ]);
  });

  testWidgets('options: a switch, and chips for one with choices', (tester) async {
    final camera = {
      'id': 'cam',
      'name': 'Cam',
      'capabilities': {
        'options': {
          'options': [
            {'id': 'privacy', 'name': 'Privacy mode', 'on': false},
            {'id': 'night_vision', 'name': 'Night vision', 'on': false, 'value': 'auto', 'choices': ['auto', 'on', 'off']},
          ],
        },
      },
    };
    final fake = FakeHub([camera]);
    final hub = await hubWith(fake);
    await tester.pumpWidget(app(CapabilityControls(device: Device.fromJson(camera), hub: hub)));

    await tester.tap(find.text('Privacy mode'));
    await tester.tap(find.text('On'));
    await tester.pump();
    expect(fake.actions().map((a) => [a['capability'], a['name'], a['args']]).toList(), [
      ['options', 'set', {'id': 'privacy', 'on': true}],
      ['options', 'set', {'id': 'night_vision', 'value': 'on'}],
    ]);
  });

  testWidgets('a camera without a stream account says how to get pictures', (tester) async {
    final camera = {
      'id': 'cam',
      'name': 'Cam',
      'capabilities': {
        'camera': {'snapshot': false, 'stream': false},
      },
    };
    final fake = FakeHub([camera]);
    final hub = await hubWith(fake);
    await tester.pumpWidget(app(CapabilityControls(device: Device.fromJson(camera), hub: hub)));
    expect(find.textContaining('No pictures'), findsOneWidget);
    expect(fake.actions(), isEmpty, reason: 'no snapshot asked for');
  });

  testWidgets('"powered by": the plugs offered, and the link sent as set_power_link', (tester) async {
    final strip = {'id': 'strip', 'name': 'LED strip', 'source': 'ir-blaster', 'capabilities': {'switch': {'on': false}}};
    final shelly = {
      'id': 'shelly',
      'name': 'Shelly',
      'source': 'http',
      'capabilities': {
        'switch': {'on': true},
        'energy': {'power_w': 3.1},
      },
    };
    final lamp = {'id': 'lamp', 'name': 'Lamp', 'source': 'wled', 'capabilities': {'switch': {'on': true}}};
    final fake = FakeHub([strip, shelly, lamp]);
    final hub = await hubWith(fake);
    expect(PowerLinkSection.applies(hub.device('strip')!), isTrue);
    expect(PowerLinkSection.applies(hub.device('shelly')!), isFalse);

    await tester.pumpWidget(app(PowerLinkSection(device: hub.device('strip')!, hub: hub)));
    expect(find.text('Not plugged into a plug the hub knows'), findsOneWidget);
    await tester.tap(find.text('None'));
    await tester.pumpAndSettle();
    // A plug that measures; not a light, nor the IR device itself.
    expect(find.text('Shelly · measures power'), findsOneWidget);
    expect(find.text('Lamp'), findsNothing);
    await tester.tap(find.text('Shelly · measures power'));
    await tester.pumpAndSettle();
    final link = fake.sent.lastWhere((m) => m['action'] == 'set_power_link');
    expect((link['id'], link['plug'], link['cut_power']), ('strip', 'shelly', false));
  });

  testWidgets('wizard: a vendor login -- saved account, then pick a device', (tester) async {
    final fake = FakeHub([]);
    final hub = await hubWith(fake);
    Map<String, dynamic> login(String phase, Map<String, dynamic> more) => {
          'type': 'wizard_step',
          'session': 's1',
          'template': 'roborock-vacuum',
          'variant': '',
          'number': 1,
          'step': 'vendor_login',
          'vendor': 'roborock',
          'phase': phase,
          'hint': '',
          ...more,
        };
    fake.wizardReplies.addAll([
      login('account', {
        'fields': [
          {'id': 'email', 'type': 'text', 'label': 'Email', 'hint': '', 'required': true},
          {'id': 'password', 'type': 'secret', 'label': 'Password', 'hint': '', 'required': true},
          {'id': 'map', 'type': 'toggle', 'label': 'Show the map', 'hint': '', 'required': false, 'value': 'true'},
        ],
        'accounts': [
          {'id': 'roborock:me@example.com', 'label': 'me@example.com'},
        ],
      }),
      login('pick', {
        'fields': [],
        'devices': [
          {'id': 'D1', 'name': 'Roborock S7', 'detail': 'online', 'available': true},
        ],
      }),
      {'type': 'wizard_step', 'session': 's1', 'number': 2, 'step': 'name', 'name': 'Roborock S7', 'room': '', 'summary': ''},
    ]);
    await tester.pumpWidget(MaterialApp(
      theme: tokens.theme(mode: 'dark', accent: 'sky', density: 'comfortable'),
      home: WizardScreen(hub: hub, start: const {'action': 'wizard_start', 'template': 'roborock-vacuum'}, title: 'Add'),
    ));
    await tester.pumpAndSettle();
    expect(find.text('Signed in: use it'), findsOneWidget);
    expect(find.text('Or sign in with another account:'), findsOneWidget);

    await tester.tap(find.text('me@example.com'));
    await tester.pumpAndSettle();
    final first = fake.sent.lastWhere((m) => m['action'] == 'wizard_answer');
    // The account -- and the screen's own choice (the map); no password.
    expect(first['values'], {'account': 'roborock:me@example.com', 'map': 'true'});

    expect(find.text('Roborock S7'), findsOneWidget);
    await tester.tap(find.text('Roborock S7'));
    await tester.pumpAndSettle();
    expect(fake.sent.lastWhere((m) => m['action'] == 'wizard_answer')['values'], {'device': 'D1'});
    expect(find.text('Add device'), findsOneWidget);
  });
}
