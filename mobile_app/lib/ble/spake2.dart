import 'dart:math';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';

/// SPAKE2 over Ed25519, side A -- byte-compatible with the Rust `spake2`
/// crate the hub uses (backend_daemon's ble.rs, `Spake2::<Ed25519Group>`)
/// and with python-spake2 (tools/ble_setup.py). Bluetooth setup (#36):
/// both sides derive the same key only if both used the same code, and the
/// code itself never travels.
///
///   start()            -> 33 bytes: 'A' + X,  X = x*B + pw*M
///   finish(33 bytes)   -> 32-byte key, from 'B' + Y:
///                         K = x*(Y - pw*N)
///                         key = SHA256(SHA256(pw) | SHA256(idA) |
///                                      SHA256(idB) | X | Y | K)
///
/// Plain BigInt arithmetic: three scalar multiplications per setup, far
/// below a second on a phone. Not constant-time -- acceptable here: the
/// secret scalar lives for one attempt, and a timing attacker would have to
/// be measuring the phone's CPU from Bluetooth range.
class Spake2A extends Spake2 {
  Spake2A(super.password, {required super.idA, required super.idB, super.scalarForTest}) : super._(sideA: true);
}

/// Side B: what the hub runs. Only used by the tests (a fake hub).
class Spake2B extends Spake2 {
  Spake2B(super.password, {required super.idA, required super.idB, super.scalarForTest}) : super._(sideA: false);
}

abstract class Spake2 {
  Spake2._(Uint8List password, {required Uint8List idA, required Uint8List idB, BigInt? scalarForTest, required bool sideA})
      : _pw = password,
        _idA = idA,
        _idB = idB,
        _sideA = sideA,
        _pwScalar = passwordToScalar(password),
        _x = scalarForTest ?? _randomScalar();

  final Uint8List _pw, _idA, _idB;
  final bool _sideA;
  final BigInt _pwScalar;
  final BigInt _x;
  Uint8List? _myMsg;

  int get _mySide => _sideA ? 0x41 : 0x42;
  Ed25519Point get _blinding => _sideA ? Ed25519Point.m : Ed25519Point.n;
  Ed25519Point get _unblinding => _sideA ? Ed25519Point.n : Ed25519Point.m;

  /// The outbound message: the side byte ('A' / 'B') + the blinded point.
  Uint8List start() {
    final elem = Ed25519Point.base.mul(_x).add(_blinding.mul(_pwScalar));
    _myMsg = elem.encode();
    return Uint8List.fromList([_mySide, ..._myMsg!]);
  }

  /// The other side's message -> the shared key. Throws [FormatException]
  /// for a malformed message.
  Uint8List finish(Uint8List message) {
    final myMsg = _myMsg;
    if (myMsg == null) throw StateError('start() first');
    final otherSide = _sideA ? 0x42 : 0x41;
    if (message.length != 33 || message[0] != otherSide) throw const FormatException('not the other side\'s SPAKE2 message');
    final otherMsg = Uint8List.sublistView(message, 1);
    final other = Ed25519Point.decode(otherMsg);
    if (_equal(other.encode(), myMsg)) throw const FormatException('reflected message');
    final k = other.add(_unblinding.mul(ed25519L - _pwScalar)).mul(_x);
    final transcript = BytesBuilder()
      ..add(sha256.convert(_pw).bytes)
      ..add(sha256.convert(_idA).bytes)
      ..add(sha256.convert(_idB).bytes)
      ..add(_sideA ? myMsg : otherMsg)
      ..add(_sideA ? otherMsg : myMsg)
      ..add(k.encode());
    return Uint8List.fromList(sha256.convert(transcript.toBytes()).bytes);
  }

  /// HKDF-SHA256(salt "", info "SPAKE2 pw") to 48 bytes, big-endian, mod L.
  static BigInt passwordToScalar(Uint8List pw) =>
      _bigEndian(hkdfSha256(pw, salt: Uint8List(0), info: 'SPAKE2 pw'.codeUnits, length: 48)) % ed25519L;

  static BigInt _randomScalar() {
    final r = Random.secure();
    return _bigEndian(Uint8List.fromList(List.generate(64, (_) => r.nextInt(256)))) % ed25519L;
  }
}

/// HKDF-SHA256 (RFC 5869).
Uint8List hkdfSha256(List<int> ikm, {required List<int> salt, required List<int> info, required int length}) {
  final prk = Hmac(sha256, salt.isEmpty ? List.filled(32, 0) : salt).convert(ikm).bytes;
  final out = BytesBuilder();
  var t = <int>[];
  for (var i = 1; out.length < length; i++) {
    t = Hmac(sha256, prk).convert([...t, ...info, i]).bytes;
    out.add(t);
  }
  return Uint8List.sublistView(out.toBytes(), 0, length);
}

final BigInt _p = (BigInt.one << 255) - BigInt.from(19);
final BigInt ed25519L = (BigInt.one << 252) + BigInt.parse('27742317777372353535851937790883648493');
final BigInt _d = (BigInt.from(-121665) * BigInt.from(121666).modInverse(_p)) % _p;
final BigInt _d2 = (_d * BigInt.two) % _p;
final BigInt _sqrtM1 = BigInt.two.modPow((_p - BigInt.one) ~/ BigInt.from(4), _p);

/// A point on edwards25519 in extended coordinates (X:Y:Z:T), x=X/Z,
/// y=Y/Z, xy=T/Z.
class Ed25519Point {
  Ed25519Point._(this.x, this.y, this.z, this.t);

  final BigInt x, y, z, t;

  static final zero = Ed25519Point._(BigInt.zero, BigInt.one, BigInt.one, BigInt.zero);

  static final base = () {
    final y = (BigInt.from(4) * BigInt.from(5).modInverse(_p)) % _p;
    return _fromAffine(_recoverX(y, 0)!, y);
  }();

  /// The blinding points of python-spake2 / the Rust crate (their
  /// arbitrary_element(b"M") and (b"N")), as the Rust crate hard-codes them.
  static final m = decode(_hex('15cfd18e385952982b6a8f8c7854963b58e34388c8e6dae891db756481a02312'));
  static final n = decode(_hex('f04f2e7eb734b2a8f8b472eaf9c3c632576ac64aea650b496a8a20ff00e583c3'));

  static Ed25519Point _fromAffine(BigInt x, BigInt y) => Ed25519Point._(x, y, BigInt.one, (x * y) % _p);

  static BigInt? _recoverX(BigInt y, int sign) {
    final y2 = (y * y) % _p;
    final x2 = ((y2 - BigInt.one) * ((_d * y2 + BigInt.one) % _p).modInverse(_p)) % _p;
    if (x2 == BigInt.zero) return sign == 0 ? BigInt.zero : null;
    var x = x2.modPow((_p + BigInt.from(3)) ~/ BigInt.from(8), _p);
    if ((x * x - x2) % _p != BigInt.zero) x = (x * _sqrtM1) % _p;
    if ((x * x - x2) % _p != BigInt.zero) return null;
    if (x.isOdd != (sign == 1)) x = _p - x;
    return x;
  }

  Ed25519Point add(Ed25519Point o) {
    final a = ((y - x) * (o.y - o.x)) % _p;
    final b = ((y + x) * (o.y + o.x)) % _p;
    final c = (t * _d2 % _p * o.t) % _p;
    final dd = (z * BigInt.two * o.z) % _p;
    final e = b - a, f = dd - c, g = dd + c, h = b + a;
    return Ed25519Point._((e * f) % _p, (g * h) % _p, (f * g) % _p, (e * h) % _p);
  }

  Ed25519Point mul(BigInt k) {
    var result = zero;
    var addend = this;
    var s = k % ed25519L;
    while (s > BigInt.zero) {
      if (s.isOdd) result = result.add(addend);
      addend = addend.add(addend);
      s >>= 1;
    }
    return result;
  }

  /// 32 bytes: y little-endian, the top bit = x's lowest bit.
  Uint8List encode() {
    final zi = z.modInverse(_p);
    final ax = (x * zi) % _p, ay = (y * zi) % _p;
    final out = Uint8List(32);
    var v = ay;
    for (var i = 0; i < 32; i++) {
      out[i] = (v & BigInt.from(0xff)).toInt();
      v >>= 8;
    }
    if (ax.isOdd) out[31] |= 0x80;
    return out;
  }

  static Ed25519Point decode(Uint8List b) {
    if (b.length != 32) throw const FormatException('a point is 32 bytes');
    final sign = b[31] >> 7;
    var y = BigInt.zero;
    for (var i = 31; i >= 0; i--) {
      y = (y << 8) | BigInt.from(i == 31 ? b[i] & 0x7f : b[i]);
    }
    if (y >= _p) throw const FormatException('not a point');
    final x = _recoverX(y, sign);
    if (x == null) throw const FormatException('not on the curve');
    return _fromAffine(x, y);
  }
}

BigInt _bigEndian(List<int> bytes) => bytes.fold(BigInt.zero, (v, b) => (v << 8) | BigInt.from(b));

Uint8List _hex(String h) => Uint8List.fromList([for (var i = 0; i < h.length; i += 2) int.parse(h.substring(i, i + 2), radix: 16)]);

bool _equal(List<int> a, List<int> b) {
  if (a.length != b.length) return false;
  var diff = 0;
  for (var i = 0; i < a.length; i++) {
    diff |= a[i] ^ b[i];
  }
  return diff == 0;
}
