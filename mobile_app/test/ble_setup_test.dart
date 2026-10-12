import 'dart:convert';
import 'dart:math';
import 'dart:typed_data';

import 'package:crypto/crypto.dart' as hashes;
import 'package:cryptography/cryptography.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:house_controller_app/ble/ble_setup.dart';
import 'package:house_controller_app/ble/spake2.dart';

/// A hub's Bluetooth service as ble.rs implements it: SPAKE2 side B with
/// the code on its screen, the confirm HMAC, AES-256-GCM both ways.
class FakeHub implements BleLink {
  FakeHub(this.code, {this.joins = true});

  final String code;
  final bool joins;
  Uint8List _response = Uint8List(0);
  SecretKey? _enc;
  Map<String, dynamic>? received;
  bool _wifiPending = false;
  final _aad = utf8.encode('uc-ble-v1');

  @override
  Future<void> write(Uint8List data) async {
    final length = (data[1] << 8) | data[2];
    expect(data.length, 3 + length, reason: 'framing');
    final payload = Uint8List.sublistView(data, 3);
    switch (data[0]) {
      case 0x01:
        final b = Spake2B(Uint8List.fromList(utf8.encode(code)),
            idA: Uint8List.fromList(utf8.encode('uc-app')), idB: Uint8List.fromList(utf8.encode('uc-hub')));
        final messageB = b.start();
        final key = b.finish(payload);
        _enc = SecretKey(hkdfSha256(key, salt: _aad, info: utf8.encode('enc'), length: 32));
        final confirmKey = hkdfSha256(key, salt: _aad, info: utf8.encode('confirm'), length: 32);
        final confirm = hashes.Hmac(hashes.sha256, confirmKey).convert([...payload, ...messageB]).bytes;
        _response = Uint8List.fromList([0x01, ...messageB, ...confirm]);
      case 0x02:
        final box = SecretBox(payload.sublist(12, payload.length - 16),
            nonce: payload.sublist(0, 12), mac: Mac(payload.sublist(payload.length - 16)));
        try {
          received = jsonDecode(utf8.decode(await AesGcm.with256bits().decrypt(box, secretKey: _enc!, aad: _aad)));
        } catch (_) {
          _response = Uint8List.fromList([0xFF, ...utf8.encode('could not decrypt: was the code right?')]);
          return;
        }
        _response = Uint8List.fromList(
            [0x02, ...await _seal({'client_id': 'c-1', 'token': 'tok', 'hub_fingerprint': 'AB' * 32})]);
        _wifiPending = true;
    }
  }

  @override
  Future<Uint8List> read() async {
    final r = _response;
    if (_wifiPending) {
      // The next read already sees the WiFi result.
      _wifiPending = false;
      _response = Uint8List.fromList(
          [0x03, ...await _seal(joins ? {'wifi': 'joined'} : {'wifi': 'failed', 'error': 'wrong password'})]);
    }
    return r;
  }

  Future<List<int>> _seal(Map<String, dynamic> j) async {
    final nonce = List.generate(12, (_) => Random.secure().nextInt(256));
    final box = await AesGcm.with256bits().encrypt(utf8.encode(jsonEncode(j)), secretKey: _enc!, nonce: nonce, aad: _aad);
    return [...nonce, ...box.cipherText, ...box.mac.bytes];
  }
}

void main() {
  test('full setup against a hub speaking ble.rs', () async {
    final hub = FakeHub('212570');
    final p = BleSetupProtocol(hub, pollInterval: Duration.zero);
    await p.exchangeKeys('212570');
    final r = await p.sendWifi(ssid: 'Home', password: 'secret', clientName: 'Phone app (test)');
    expect(hub.received, {'ssid': 'Home', 'password': 'secret', 'client_name': 'Phone app (test)'});
    expect((r.clientId, r.token, r.hubFingerprint), ('c-1', 'tok', 'ab' * 32));
    expect(await p.wifiResult(), isNull);
  });

  test('a wrong code stops before anything secret is sent', () async {
    final hub = FakeHub('212570');
    final p = BleSetupProtocol(hub, pollInterval: Duration.zero);
    await expectLater(p.exchangeKeys('999999'), throwsA(isA<BleSetupException>()));
    expect(hub.received, isNull);
  });

  test('the WiFi failure reason comes through', () async {
    final hub = FakeHub('111111', joins: false);
    final p = BleSetupProtocol(hub, pollInterval: Duration.zero);
    await p.exchangeKeys('111111');
    await p.sendWifi(ssid: 'Home', password: 'nope', clientName: 'x');
    expect(await p.wifiResult(), 'wrong password');
  });

  test('framing is [type][length BE][payload]', () {
    expect(BleSetupProtocol.frame(2, List.filled(300, 7)).sublist(0, 3), [2, 1, 44]);
  });
}
