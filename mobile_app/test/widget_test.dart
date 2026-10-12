import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:house_controller_app/hub/device.dart';
import 'package:house_controller_app/hub/hub_address.dart';
import 'package:house_controller_app/hub/hub_store.dart';
import 'package:house_controller_app/screens/home_screen.dart';
import 'package:house_controller_app/theme/tokens.dart';
import 'package:house_controller_app/widgets/capability_controls.dart';

const _fp = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef';

void main() {
  // Loaded once, outside any testWidgets: real asset loading never finishes
  // inside a widget test's fake clock.
  late DesignTokens tokens;
  setUpAll(() async {
    TestWidgetsFlutterBinding.ensureInitialized();
    tokens = await DesignTokens.load();
  });

  group('pairing QR code (ui_layer show_pairing)', () {
    test('reads what the hub shows', () {
      final i = PairingInvite.fromUri('uchub://pair?host=192.168.1.20&port=8443&code=212570&fp=${_fp.toUpperCase()}')!;
      expect((i.host, i.port, i.code, i.fingerprint), ('192.168.1.20', 8443, '212570', _fp));
    });

    test('refuses anything else', () {
      expect(PairingInvite.fromUri('https://example.com'), isNull);
      expect(PairingInvite.fromUri('uchub://pair?host=h&port=8443&code=12345&fp=$_fp'), isNull, reason: '5 digits');
      expect(PairingInvite.fromUri('uchub://pair?host=h&port=8443&code=123456&fp=abc'), isNull, reason: 'short fp');
      expect(PairingInvite.fromUri('uchub://pair?port=8443&code=123456&fp=$_fp'), isNull, reason: 'no host');
    });

    test('short fingerprint as on the hub screen (ws.rs fingerprint_short)', () {
      expect(shortFingerprint(_fp), '0123 4567 89ab cdef');
    });

    test('typed addresses', () {
      expect(splitHostPort('stm32mp1.local'), ('stm32mp1.local', 8443));
      expect(splitHostPort(' 192.168.1.20:9443 '), ('192.168.1.20', 9443));
    });
  });

  group('device model (device.rs)', () {
    test('keeps capabilities this app does not know', () {
      final d = Device.fromJson({
        'id': 'robo',
        'name': 'Vacuum',
        'capabilities': {
          'switch': {'on': true},
          'vacuum': {'status': 'cleaning', 'battery': 80},
        },
      });
      expect(d.isOn, isTrue);
      expect(d.cap('vacuum')!['battery'], 80);
      expect(d.reachable, isTrue, reason: 'online unknown is not greyed out');
    });

    test('summary says why a device is greyed out', () {
      final d = Device.fromJson({'id': 'tv', 'name': 'TV', 'online': 'unauthorized', 'capabilities': {}});
      expect(d.reachable, isFalse);
      expect(summaryOf(d), contains('pairing again'));
    });
  });

  group('design tokens (tokens.json, #39)', () {
    test('preset colours come from the hub file', () {
      final t = tokens.theme(mode: 'dark', accent: 'violet', density: 'compact');
      expect(t.colorScheme.primary, const Color(0xFFA78BFA));
      expect(t.scaffoldBackgroundColor, const Color(0xFF0F172A));
      expect(t.extension<Spacing>()!.l, 8);
    });

    test('auto follows the hours in tokens.json', () {
      expect(tokens.resolveMode('auto', DateTime(2026, 1, 1, 12)), 'light');
      expect(tokens.resolveMode('auto', DateTime(2026, 1, 1, 22)), 'dark');
      expect(tokens.resolveMode('nonsense', DateTime(2026)), 'dark');
    });
  });

  testWidgets('controls are built from capabilities', (tester) async {
    final d = Device.fromJson({
      'id': 'x',
      'name': 'Everything',
      'capabilities': {
        'switch': {'on': false},
        'dimmer': {'level': 40},
        'lock': {'state': 'locked'},
        'climate': {'mode': 'heat', 'target': 21.5, 'current': 20.0, 'modes': ['off', 'heat']},
        'energy': {'power_w': 40.2},
        'irrigation': {'zone': 'lawn'},
      },
    });
    await tester.pumpWidget(MaterialApp(
      theme: tokens.theme(mode: 'dark', accent: 'sky', density: 'comfortable'),
      home: Scaffold(body: SingleChildScrollView(child: CapabilityControls(device: d, hub: HubStore()))),
    ));
    expect(find.text('Off'), findsWidgets);
    expect(find.text('40 %'), findsOneWidget);
    expect(find.text('Unlock…'), findsOneWidget);
    expect(find.text('21.5 °C'), findsOneWidget);
    expect(find.text('40.2 W'), findsOneWidget);
    expect(find.text('Irrigation'), findsOneWidget, reason: 'unknown capability shown read-only');
  });
}
