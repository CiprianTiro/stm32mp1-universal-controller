/// Where a hub is, and what the pairing QR code on its screen says.
///
/// The QR code (ui_layer's show_pairing) is
///   uchub://pair?host=192.168.1.20&port=8443&code=212570&fp=`<sha256 hex>`
/// -- where to connect, the one-time code, and the certificate fingerprint
/// to pin (backend_daemon's tls.rs). With it, nothing has to be compared by
/// eye: the app knows which certificate to expect before it connects.
library;

/// The LAN door of ws.rs: wss, the hub's own certificate, pairing needed.
const lanPort = 8443;

/// The name avahi gives the hub. The hub doesn't announce a service of its
/// own (yet), so without a QR code this is the first address to try.
const defaultHubHost = 'stm32mp1.local';

class PairingInvite {
  PairingInvite({required this.host, required this.port, required this.code, this.fingerprint});

  final String host;
  final int port;
  final String code;

  /// Lowercase hex SHA-256 of the hub's certificate; null when the code was
  /// typed (then the person compares the short form with the hub's screen).
  final String? fingerprint;

  /// Parses a scanned QR code; null if it isn't a hub pairing code.
  static PairingInvite? fromUri(String text) {
    final uri = Uri.tryParse(text.trim());
    if (uri == null || uri.scheme != 'uchub' || uri.host != 'pair') return null;
    final q = uri.queryParameters;
    final host = q['host'] ?? '';
    final code = q['code'] ?? '';
    final fp = (q['fp'] ?? '').toLowerCase();
    final port = int.tryParse(q['port'] ?? '') ?? lanPort;
    if (host.isEmpty || !RegExp(r'^\d{6}$').hasMatch(code)) return null;
    if (!RegExp(r'^[0-9a-f]{64}$').hasMatch(fp)) return null;
    return PairingInvite(host: host, port: port, code: code, fingerprint: fp);
  }
}

/// The first 16 hex digits in groups of four, as the hub's pairing screen
/// shows them (ws.rs's fingerprint_short), for comparing by eye.
String shortFingerprint(String fingerprint) {
  final head = fingerprint.length >= 16 ? fingerprint.substring(0, 16) : fingerprint;
  return [for (var i = 0; i < head.length; i += 4) head.substring(i, (i + 4).clamp(0, head.length))].join(' ');
}

/// Splits "host" or "host:port" as typed by a person.
(String, int) splitHostPort(String text) {
  final t = text.trim();
  final colon = t.lastIndexOf(':');
  if (colon > 0 && !t.contains(']') && t.indexOf(':') == colon) {
    final port = int.tryParse(t.substring(colon + 1));
    if (port != null) return (t.substring(0, colon), port);
  }
  return (t, lanPort);
}
