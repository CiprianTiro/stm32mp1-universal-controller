import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';
import 'dart:typed_data';

import 'package:crypto/crypto.dart' as hashes;
import 'package:cryptography/cryptography.dart';
import 'package:flutter_reactive_ble/flutter_reactive_ble.dart';
import 'package:permission_handler/permission_handler.dart';

import 'spake2.dart';

/// First-time hub setup over Bluetooth (#36): the home WiFi's name and
/// password go to the hub, and the phone is paired (#35) in the same step.
/// The protocol is backend_daemon's ble.rs; tools/ble_setup.py is the
/// reference client this follows step by step.
///
///   RX 0x01 START  'A'+X (SPAKE2, the code from the hub's screen)
///   TX 0x01        'B'+Y + HMAC(confirm key, A||B)  -- checked BEFORE
///                  anything secret is sent: a wrong code stops here
///   RX 0x02 SETUP  nonce + AES-GCM({"ssid", "password", "client_name"})
///   TX 0x02        nonce + AES-GCM({"client_id", "token", "hub_fingerprint"})
///   TX 0x03        nonce + AES-GCM({"wifi": "joined" | "failed", "error"?})
///   TX 0xFF        an error, plain text
///
/// Requests are framed [type][length, 2 bytes big-endian][payload]; answers
/// are read from TX until the expected type is there.
const bleServiceUuid = '7f1c0000-5a3b-4c6e-9d2a-1b3c5d7e9f00';
const bleRxUuid = '7f1c0001-5a3b-4c6e-9d2a-1b3c5d7e9f00';
const bleTxUuid = '7f1c0002-5a3b-4c6e-9d2a-1b3c5d7e9f00';

final _saltAad = utf8.encode('uc-ble-v1');
final _idApp = Uint8List.fromList(utf8.encode('uc-app'));
final _idHub = Uint8List.fromList(utf8.encode('uc-hub'));

/// What the hub's Bluetooth service is to the protocol: write a request,
/// read the current answer. Real Bluetooth below; a fake hub in the tests.
abstract class BleLink {
  Future<void> write(Uint8List data);
  Future<Uint8List> read();
}

class BleSetupResult {
  BleSetupResult({required this.clientId, required this.token, required this.hubFingerprint});
  final String clientId;
  final String token;
  final String hubFingerprint;
}

class BleSetupException implements Exception {
  const BleSetupException(this.message);
  final String message;
  @override
  String toString() => message;
}

/// The protocol, over any [BleLink]. One instance per attempt.
class BleSetupProtocol {
  BleSetupProtocol(this._link, {this.pollInterval = const Duration(milliseconds: 500)});

  final BleLink _link;
  final Duration pollInterval;
  SecretKey? _enc;

  static Uint8List frame(int kind, List<int> payload) =>
      Uint8List.fromList([kind, payload.length >> 8, payload.length & 0xff, ...payload]);

  /// Step 1: the key exchange. Throws [BleSetupException] for a wrong code
  /// -- nothing secret has been sent then.
  Future<void> exchangeKeys(String code, {BigInt? scalarForTest}) async {
    final spake = Spake2A(Uint8List.fromList(utf8.encode(code)), idA: _idApp, idB: _idHub, scalarForTest: scalarForTest);
    final messageA = spake.start();
    final answer = await _request(0x01, messageA, expect: 0x01);
    if (answer.length != 33 + 32) throw const BleSetupException('unexpected answer from the hub');
    final messageB = Uint8List.sublistView(answer, 0, 33);
    final Uint8List key;
    try {
      key = spake.finish(messageB);
    } on FormatException catch (e) {
      throw BleSetupException('the hub sent a malformed key exchange message ($e)');
    }
    final enc = hkdfSha256(key, salt: _saltAad, info: utf8.encode('enc'), length: 32);
    final confirmKey = hkdfSha256(key, salt: _saltAad, info: utf8.encode('confirm'), length: 32);
    final expected = hashes.Hmac(hashes.sha256, confirmKey).convert([...messageA, ...messageB]).bytes;
    if (!_constantTimeEqual(expected, Uint8List.sublistView(answer, 33))) {
      throw const BleSetupException('Wrong code: the hub derived a different key. Nothing secret was sent.');
    }
    _enc = SecretKey(enc);
  }

  /// Step 2: the WiFi details -> paired.
  Future<BleSetupResult> sendWifi({required String ssid, required String password, required String clientName}) async {
    final sealed = await _seal(utf8.encode(jsonEncode({'ssid': ssid, 'password': password, 'client_name': clientName})));
    final answer = await _request(0x02, sealed, expect: 0x02);
    final j = jsonDecode(utf8.decode(await _open(answer))) as Map<String, dynamic>;
    return BleSetupResult(
      clientId: j['client_id'] as String,
      token: j['token'] as String,
      hubFingerprint: (j['hub_fingerprint'] as String).toLowerCase(),
    );
  }

  /// Step 3: waits for the hub to say whether it joined the WiFi. Null:
  /// joined; otherwise the reason it didn't.
  Future<String?> wifiResult({Duration timeout = const Duration(seconds: 40)}) async {
    final answer = await _poll(0x03, timeout);
    final j = jsonDecode(utf8.decode(await _open(answer))) as Map<String, dynamic>;
    return j['wifi'] == 'joined' ? null : (j['error'] as String? ?? 'unknown error');
  }

  Future<Uint8List> _request(int kind, List<int> payload, {required int expect}) async {
    await _link.write(frame(kind, payload));
    return _poll(expect, const Duration(seconds: 40));
  }

  /// Reads TX until it holds an answer of type [expect] (the hub keeps its
  /// last answer there), or an error.
  Future<Uint8List> _poll(int expect, Duration timeout) async {
    final deadline = DateTime.now().add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      final answer = await _link.read();
      if (answer.isNotEmpty && answer[0] == 0xFF) {
        throw BleSetupException('Hub: ${utf8.decode(answer.sublist(1), allowMalformed: true)}');
      }
      if (answer.isNotEmpty && answer[0] == expect) return Uint8List.sublistView(answer, 1);
      await Future<void>.delayed(pollInterval);
    }
    throw const BleSetupException('No answer from the hub in time.');
  }

  Future<Uint8List> _seal(List<int> plain) async {
    final r = Random.secure();
    final nonce = List.generate(12, (_) => r.nextInt(256));
    final box = await AesGcm.with256bits().encrypt(plain, secretKey: _enc!, nonce: nonce, aad: _saltAad);
    return Uint8List.fromList([...nonce, ...box.cipherText, ...box.mac.bytes]);
  }

  Future<List<int>> _open(Uint8List data) async {
    if (data.length < 12 + 16) throw const BleSetupException('unexpected answer from the hub');
    final box = SecretBox(
      data.sublist(12, data.length - 16),
      nonce: data.sublist(0, 12),
      mac: Mac(data.sublist(data.length - 16)),
    );
    try {
      return await AesGcm.with256bits().decrypt(box, secretKey: _enc!, aad: _saltAad);
    } on SecretBoxAuthenticationError {
      throw const BleSetupException('the hub\'s answer could not be decrypted');
    }
  }

  static bool _constantTimeEqual(List<int> a, List<int> b) {
    if (a.length != b.length) return false;
    var diff = 0;
    for (var i = 0; i < a.length; i++) {
      diff |= a[i] ^ b[i];
    }
    return diff == 0;
  }
}

/// The real Bluetooth side: finding the hub and its two characteristics.
class HubBluetooth {
  HubBluetooth([FlutterReactiveBle? ble]) : _ble = ble ?? FlutterReactiveBle();

  final FlutterReactiveBle _ble;
  StreamSubscription<ConnectionStateUpdate>? _connection;

  static final _service = Uuid.parse(bleServiceUuid);
  static final _rx = Uuid.parse(bleRxUuid);
  static final _tx = Uuid.parse(bleTxUuid);

  /// Android 12+ asks for "Nearby devices"; older Android needs location
  /// to scan. iOS asks by itself on first use (Info.plist's text).
  Future<void> ensurePermissions() async {
    if (!Platform.isAndroid) return;
    final statuses = await [Permission.bluetoothScan, Permission.bluetoothConnect, Permission.locationWhenInUse].request();
    // (On Android 11 and older the two Bluetooth ones are granted by
    // themselves and location is what scanning needs.)
    if (!statuses[Permission.bluetoothScan]!.isGranted || !statuses[Permission.bluetoothConnect]!.isGranted) {
      throw const BleSetupException('Bluetooth permission is needed to find the hub (Settings → Apps → permissions).');
    }
  }

  /// Finds a hub in setup mode (it is only visible while a pairing code is
  /// shown on its screen) and connects. Returns the link to talk over.
  Future<BleLink> connect({Duration scanFor = const Duration(seconds: 15)}) async {
    final status = await _ble.statusStream.firstWhere((s) => s != BleStatus.unknown);
    if (status == BleStatus.poweredOff) throw const BleSetupException('Bluetooth is off: switch it on and try again.');
    if (status == BleStatus.unauthorized) throw const BleSetupException('Bluetooth permission was refused.');
    if (status == BleStatus.unsupported) throw const BleSetupException('This phone has no Bluetooth LE.');

    final DiscoveredDevice device;
    try {
      device = await _ble
          .scanForDevices(withServices: [_service], requireLocationServicesEnabled: false)
          .first
          .timeout(scanFor);
    } on TimeoutException {
      throw const BleSetupException('No hub found. Is the pairing code shown on the hub\'s screen?');
    }

    final connected = Completer<void>();
    _connection = _ble
        .connectToDevice(id: device.id, connectionTimeout: const Duration(seconds: 20))
        .listen((update) {
      if (update.connectionState == DeviceConnectionState.connected && !connected.isCompleted) connected.complete();
      if (update.connectionState == DeviceConnectionState.disconnected && !connected.isCompleted) {
        connected.completeError(const BleSetupException('The hub refused the Bluetooth connection.'));
      }
    }, onError: (Object e) {
      if (!connected.isCompleted) connected.completeError(BleSetupException('Bluetooth: $e'));
    });
    await connected.future.timeout(const Duration(seconds: 25),
        onTimeout: () => throw const BleSetupException('Connecting to the hub took too long.'));
    if (Platform.isAndroid) {
      // Room for the whole SETUP request in one write (iOS negotiates by
      // itself); the hub also reassembles split writes, so failing is fine.
      try {
        await _ble.requestMtu(deviceId: device.id, mtu: 247);
      } catch (_) {}
    }
    return _ReactiveLink(
      _ble,
      QualifiedCharacteristic(serviceId: _service, characteristicId: _rx, deviceId: device.id),
      QualifiedCharacteristic(serviceId: _service, characteristicId: _tx, deviceId: device.id),
    );
  }

  Future<void> disconnect() async {
    await _connection?.cancel();
    _connection = null;
  }
}

class _ReactiveLink implements BleLink {
  _ReactiveLink(this._ble, this._rx, this._tx);
  final FlutterReactiveBle _ble;
  final QualifiedCharacteristic _rx, _tx;

  @override
  Future<void> write(Uint8List data) => _ble.writeCharacteristicWithResponse(_rx, value: data);

  @override
  Future<Uint8List> read() async => Uint8List.fromList(await _ble.readCharacteristic(_tx));
}
