import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:house_controller_app/ble/spake2.dart';

/// A fixed vector from python-spake2 (tools/ble_setup.py's library, which
/// the hub's Rust crate interoperates with): code "212570", identities
/// "uc-app" / "uc-hub", side A's secret scalar fixed.
Uint8List _hex(String h) => Uint8List.fromList([for (var i = 0; i < h.length; i += 2) int.parse(h.substring(i, i + 2), radix: 16)]);
String _toHex(List<int> b) => b.map((x) => x.toRadixString(16).padLeft(2, '0')).join();

void main() {
  final pw = Uint8List.fromList(utf8.encode('212570'));
  final idA = Uint8List.fromList(utf8.encode('uc-app'));
  final idB = Uint8List.fromList(utf8.encode('uc-hub'));

  test('password scalar', () {
    expect(Spake2.passwordToScalar(pw), BigInt.parse('eaf68a0314bf8199c12044429be706208d18c56a37cb5c880bc3d8a6ccb1080', radix: 16));
  });

  test('blinding points decode and re-encode', () {
    expect(_toHex(Ed25519Point.m.encode()), '15cfd18e385952982b6a8f8c7854963b58e34388c8e6dae891db756481a02312');
    expect(_toHex(Ed25519Point.n.encode()), 'f04f2e7eb734b2a8f8b472eaf9c3c632576ac64aea650b496a8a20ff00e583c3');
    expect(_toHex(Ed25519Point.base.encode()), '5866666666666666666666666666666666666666666666666666666666666666');
  });

  test('same messages and key as python-spake2', () {
    final a = Spake2A(pw, idA: idA, idB: idB,
        scalarForTest: BigInt.parse('d615c3c94edebb02f759435808146b240e5a24e71c1ac3da41c161bc64b66ab', radix: 16));
    expect(_toHex(a.start()), '419e97cfe7e442783ad3688a9b809408f00b07ee274a0ce75f7e8395109393aa34');
    final key = a.finish(_hex('421baaf845648eaf78b5d568c6920d2659c8faa866f0168ee954c714d643a0cc46'));
    expect(_toHex(key), 'bf0e753e4cc9d8c948f841bf2229235ade00ed1fcc3e70efb1a87ce4e12c35cb');
  });

  test('a wrong code gives another key', () {
    final a = Spake2A(Uint8List.fromList(utf8.encode('000000')), idA: idA, idB: idB,
        scalarForTest: BigInt.parse('d615c3c94edebb02f759435808146b240e5a24e71c1ac3da41c161bc64b66ab', radix: 16));
    a.start();
    final key = a.finish(_hex('421baaf845648eaf78b5d568c6920d2659c8faa866f0168ee954c714d643a0cc46'));
    expect(_toHex(key), isNot('bf0e753e4cc9d8c948f841bf2229235ade00ed1fcc3e70efb1a87ce4e12c35cb'));
  });

  test('refuses malformed messages', () {
    final a = Spake2A(pw, idA: idA, idB: idB)..start();
    expect(() => a.finish(Uint8List(33)), throwsFormatException, reason: 'side byte');
    expect(() => a.finish(Uint8List(5)), throwsFormatException, reason: 'length');
  });

  test('HKDF-SHA256 matches RFC 5869 test case 1', () {
    final okm = hkdfSha256(List.filled(22, 0x0b), salt: _hex('000102030405060708090a0b0c'), info: _hex('f0f1f2f3f4f5f6f7f8f9'), length: 42);
    expect(_toHex(okm), '3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865');
  });
}
