import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';

/// One WebSocket connection to the hub's LAN door, `wss://<hub>:8443/ws`
/// (backend_daemon's ws.rs, protocol v2).
///
/// THE CERTIFICATE. The hub makes its own certificate (tls.rs), so no
/// public authority vouches for it. Instead it is PINNED: pairing gives us
/// its SHA-256, and every later connection must present exactly that one --
/// checked during the TLS handshake, before a single byte (the key!) is
/// sent. Same rule as tools/hub_ws.py.
///
/// MESSAGES. Every request {"action": ...} gets exactly one reply
/// {"type": ...}, in order; so replies are matched to requests by a FIFO
/// queue. After "subscribe" the hub also pushes events, told apart from
/// replies by their type (see [eventTypes]).
class HubConnection {
  HubConnection._(this._socket, this.presentedFingerprint) {
    _socket.listen(_onMessage, onDone: _onDone, onError: (_) => _onDone(), cancelOnError: true);
  }

  /// The types the hub PUSHES after subscribe (ws.rs's ServerMessage
  /// "Events"); everything else answers a request.
  static const eventTypes = {
    'device_changed',
    'device_removed',
    'events_lost',
    'settings_changed',
    'found_changed',
    'automations_changed',
    'automation_ran',
  };

  /// How long a reply may take. Generous: run_scene answers once every
  /// step has been carried out, and a device adapter may be slow.
  static const replyTimeout = Duration(seconds: 30);

  final WebSocket _socket;

  /// SHA-256 (lowercase hex) of the certificate the hub presented.
  final String presentedFingerprint;

  final _pending = Queue<Completer<Map<String, dynamic>>>();
  final _events = StreamController<Map<String, dynamic>>.broadcast();
  final _closed = Completer<void>();

  Stream<Map<String, dynamic>> get events => _events.stream;

  /// Completes when the connection is gone, for whatever reason.
  Future<void> get done => _closed.future;

  bool get isOpen => !_closed.isCompleted;

  /// Connects. With [expectFingerprint], a hub presenting any other
  /// certificate is refused during the handshake ([CertificateMismatch]).
  /// Without it (pairing by typed code) any certificate is accepted, and
  /// [presentedFingerprint] says which one it was -- to show it, and to
  /// compare it with what `pair` reports.
  static Future<HubConnection> open(String host, int port, {String? expectFingerprint}) async {
    String? presented;
    // No trusted roots at all: every certificate goes through the callback,
    // even one a public authority signed -- the pin is the only test.
    final client = HttpClient(context: SecurityContext(withTrustedRoots: false))
      ..connectionTimeout = const Duration(seconds: 10)
      ..badCertificateCallback = (cert, _, _) {
        presented = sha256.convert(cert.der).toString();
        return expectFingerprint == null || presented == expectFingerprint;
      };
    final url = Uri(scheme: 'wss', host: host, port: port, path: '/ws');
    try {
      final socket = await WebSocket.connect(url.toString(), customClient: client)
          .timeout(const Duration(seconds: 15));
      socket.pingInterval = const Duration(seconds: 30);
      if (presented == null) {
        // Can't happen with no trusted roots; refuse rather than trust
        // something that wasn't checked.
        await socket.close();
        throw const HubException('the hub presented no certificate to check');
      }
      return HubConnection._(socket, presented!);
    } on HandshakeException {
      if (expectFingerprint != null && presented != null && presented != expectFingerprint) {
        throw CertificateMismatch(host);
      }
      rethrow;
    } finally {
      client.close();
    }
  }

  /// Sends one request and waits for its reply. A reply of type "error"
  /// is thrown as a [HubException] with the hub's message.
  Future<Map<String, dynamic>> request(Map<String, dynamic> message, {Duration? timeout}) {
    if (!isOpen) return Future.error(const HubException('not connected to the hub'));
    final completer = Completer<Map<String, dynamic>>();
    _pending.add(completer);
    _socket.add(jsonEncode(message));
    return completer.future.timeout(timeout ?? replyTimeout, onTimeout: () {
      // The reply may still come; the queue must stay in step with the hub,
      // so the completer stays in it (and is skipped when it does).
      throw const HubException('the hub did not answer in time');
    });
  }

  /// Pairing (#35): the code from the hub's screen, and a name for this
  /// phone as the hub's list of paired devices will show it.
  Future<PairResult> pair(String code, String clientName) async {
    final reply = await request({'action': 'pair', 'code': code, 'client_name': clientName});
    if (reply['type'] != 'paired') throw HubException('pairing failed: ${reply['message'] ?? reply['type']}');
    final result = PairResult(
      clientId: reply['client_id'] as String,
      token: reply['token'] as String,
      hubFingerprint: (reply['hub_fingerprint'] as String).toLowerCase(),
    );
    if (result.hubFingerprint != presentedFingerprint) {
      // Whoever answered presented a certificate other than the hub's own:
      // someone in between. Forget the key we just got.
      throw const HubException('pairing refused: the hub reports a different certificate than it presented');
    }
    return result;
  }

  /// Logs in with the key from pairing.
  Future<void> authenticate(String token) async {
    final reply = await request({'action': 'auth', 'token': token});
    if (reply['type'] != 'authenticated') throw HubRefused('login refused: ${reply['message'] ?? reply['type']}');
  }

  Future<void> close() async {
    await _socket.close();
    _onDone();
  }

  void _onMessage(dynamic data) {
    if (data is! String) return;
    final Map<String, dynamic> message;
    try {
      message = jsonDecode(data) as Map<String, dynamic>;
    } catch (_) {
      return;
    }
    final type = message['type'];
    if (eventTypes.contains(type)) {
      _events.add(message);
      return;
    }
    if (_pending.isEmpty) return;
    final completer = _pending.removeFirst();
    if (completer.isCompleted) return; // timed out earlier
    if (type == 'error') {
      completer.completeError(HubRefused(message['message'] as String? ?? 'refused'));
    } else {
      completer.complete(message);
    }
  }

  void _onDone() {
    if (_closed.isCompleted) return;
    _closed.complete();
    while (_pending.isNotEmpty) {
      final c = _pending.removeFirst();
      if (!c.isCompleted) c.completeError(const HubException('the connection to the hub closed'));
    }
    _events.close();
  }
}

class PairResult {
  PairResult({required this.clientId, required this.token, required this.hubFingerprint});
  final String clientId;
  final String token;
  final String hubFingerprint;
}

class HubException implements Exception {
  const HubException(this.message);
  final String message;
  @override
  String toString() => message;
}

/// The hub answered with {"type": "error"}: it got the request and said no.
class HubRefused extends HubException {
  const HubRefused(super.message);
}

/// The hub at this address presented another certificate than when we
/// paired: not the same hub, or someone in between. Nothing was sent.
class CertificateMismatch extends HubException {
  CertificateMismatch(String host)
      : super('$host presented a different certificate than when paired -- not the same hub, '
            'or someone in between. Nothing was sent.');
}
