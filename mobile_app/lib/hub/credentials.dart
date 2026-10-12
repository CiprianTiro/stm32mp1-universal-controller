import 'dart:convert';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// What pairing leaves behind: where the hub is, the key it gave us, and
/// the certificate it must keep presenting. The key is as good as a
/// password, so it lives in the platform keystore (Android Keystore / iOS
/// Keychain), never in plain preferences.
class HubCredentials {
  HubCredentials({
    required this.host,
    required this.port,
    required this.clientId,
    required this.token,
    required this.fingerprint,
  });

  final String host;
  final int port;
  final String clientId;
  final String token;
  final String fingerprint;

  Map<String, dynamic> toJson() => {
        'host': host,
        'port': port,
        'client_id': clientId,
        'token': token,
        'fingerprint': fingerprint,
      };

  static HubCredentials? fromJson(Map<String, dynamic> j) {
    final host = j['host'], port = j['port'], token = j['token'], fp = j['fingerprint'];
    if (host is! String || port is! int || token is! String || fp is! String) return null;
    return HubCredentials(
      host: host,
      port: port,
      clientId: j['client_id'] as String? ?? '',
      token: token,
      fingerprint: fp,
    );
  }

  HubCredentials withAddress(String host, int port) =>
      HubCredentials(host: host, port: port, clientId: clientId, token: token, fingerprint: fingerprint);
}

/// One paired hub per phone for now; the key is versioned so a later
/// multi-hub format can live next to it.
class CredentialStore {
  CredentialStore([FlutterSecureStorage? storage]) : _storage = storage ?? const FlutterSecureStorage();

  static const _key = 'hub.v1';
  final FlutterSecureStorage _storage;

  Future<HubCredentials?> load() async {
    final text = await _storage.read(key: _key);
    if (text == null) return null;
    try {
      return HubCredentials.fromJson(jsonDecode(text) as Map<String, dynamic>);
    } catch (_) {
      return null;
    }
  }

  Future<void> save(HubCredentials c) => _storage.write(key: _key, value: jsonEncode(c.toJson()));

  Future<void> clear() => _storage.delete(key: _key);
}
