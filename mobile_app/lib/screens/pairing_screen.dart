import 'dart:io';

import 'package:flutter/material.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

import '../hub/hub_address.dart';
import '../hub/hub_connection.dart';
import '../hub/hub_discovery.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import 'ble_setup_screen.dart';

/// Pairing with the hub (#35). On the hub's screen: Settings -> Paired
/// devices -> Pair a new device. It shows a 6-digit code and a QR code.
///
///  - Scanning the QR code is the safe way: it carries the certificate's
///    fingerprint, so the app knows the real hub before connecting.
///  - Typing the code: the app connects first, shows the certificate it
///    got, and the person compares it with the hub's screen before typing.
class PairingScreen extends StatefulWidget {
  const PairingScreen({super.key, required this.hub});

  final HubStore hub;

  @override
  State<PairingScreen> createState() => _PairingScreenState();
}

/// How this phone appears in the hub's list of paired devices.
String get clientName => 'Phone app (${Platform.operatingSystem})';

class _PairingScreenState extends State<PairingScreen> {
  bool _typing = false;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('Pair with your hub')),
      body: SafeArea(
        child: Padding(
          padding: EdgeInsets.all(sp.xl),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(
                'On the hub\'s screen open Settings → Paired devices → Pair a new device.',
                style: Theme.of(context).textTheme.bodyLarge,
              ),
              SizedBox(height: sp.l),
              SegmentedButton<bool>(
                segments: const [
                  ButtonSegment(value: false, icon: Icon(Icons.qr_code_scanner), label: Text('Scan QR code')),
                  ButtonSegment(value: true, icon: Icon(Icons.dialpad), label: Text('Type the code')),
                ],
                selected: {_typing},
                onSelectionChanged: (s) => setState(() => _typing = s.first),
              ),
              SizedBox(height: sp.l),
              Expanded(child: _typing ? _TypedPairing(hub: widget.hub) : _QrPairing(hub: widget.hub)),
              SizedBox(height: sp.s),
              TextButton.icon(
                icon: const Icon(Icons.bluetooth),
                label: const Text('New hub, not on your network yet? Set it up over Bluetooth'),
                onPressed: () => Navigator.of(context)
                    .push(MaterialPageRoute(builder: (_) => BleSetupScreen(hub: widget.hub))),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _QrPairing extends StatefulWidget {
  const _QrPairing({required this.hub});
  final HubStore hub;

  @override
  State<_QrPairing> createState() => _QrPairingState();
}

class _QrPairingState extends State<_QrPairing> {
  final _scanner = MobileScannerController(formats: [BarcodeFormat.qrCode]);
  bool _busy = false;
  String? _message;

  @override
  void dispose() {
    _scanner.dispose();
    super.dispose();
  }

  Future<void> _onDetect(BarcodeCapture capture) async {
    if (_busy) return;
    for (final code in capture.barcodes) {
      final invite = PairingInvite.fromUri(code.rawValue ?? '');
      if (invite == null) continue;
      setState(() {
        _busy = true;
        _message = 'Pairing with ${invite.host}…';
      });
      await _scanner.stop();
      try {
        await widget.hub.pairWithInvite(invite, clientName);
        // HubStore now connected: main.dart swaps this screen out.
      } catch (e) {
        if (!mounted) return;
        setState(() {
          _busy = false;
          _message = '$e';
        });
        await _scanner.start();
      }
      return;
    }
    if (!_busy && mounted) setState(() => _message = 'That isn\'t the hub\'s pairing code.');
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(
          child: ClipRRect(
            borderRadius: BorderRadius.circular(8),
            child: _busy
                ? const Center(child: CircularProgressIndicator())
                : MobileScanner(controller: _scanner, onDetect: _onDetect),
          ),
        ),
        if (_message != null) ...[SizedBox(height: sp.m), Text(_message!, textAlign: TextAlign.center)],
      ],
    );
  }
}

class _TypedPairing extends StatefulWidget {
  const _TypedPairing({required this.hub});
  final HubStore hub;

  @override
  State<_TypedPairing> createState() => _TypedPairingState();
}

class _TypedPairingState extends State<_TypedPairing> {
  final _host = TextEditingController(text: defaultHubHost);
  final _code = TextEditingController();
  HubConnection? _conn;
  String _connHost = '';
  int _connPort = lanPort;
  bool _busy = false;
  String? _error;
  List<FoundHub>? _found;

  @override
  void initState() {
    super.initState();
    _search();
  }

  /// Hubs announcing themselves on the network (mDNS): tap one instead of
  /// typing its address.
  Future<void> _search() async {
    setState(() => _found = null);
    final hubs = await findHubs();
    if (!mounted) return;
    setState(() => _found = hubs);
    if (hubs.length == 1 && _conn == null && _host.text == defaultHubHost) _host.text = hubs.first.toString();
  }

  @override
  void dispose() {
    _host.dispose();
    _code.dispose();
    // Left without pairing: don't keep an anonymous connection open.
    if (_conn != null && widget.hub.status == HubStatus.unpaired) _conn!.close();
    super.dispose();
  }

  Future<void> _connect() async {
    final (host, port) = splitHostPort(_host.text);
    if (host.isEmpty) return;
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final conn = await widget.hub.openForPairing(host, port);
      // The hub drops a connection that hasn't paired within 30 s (ws.rs).
      conn.done.then((_) {
        if (mounted && identical(_conn, conn) && widget.hub.status == HubStatus.unpaired) {
          setState(() {
            _conn = null;
            _error = 'The hub closed the connection (30 s to pair). Connect again.';
          });
        }
      });
      setState(() {
        _conn = conn;
        _connHost = host;
        _connPort = port;
      });
    } catch (e) {
      setState(() => _error = 'Can\'t reach $host: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _pair() async {
    final code = _code.text.replaceAll(RegExp(r'\s'), '');
    if (_conn == null || !RegExp(r'^\d{6}$').hasMatch(code)) {
      setState(() => _error = 'The code is the 6 digits on the hub\'s screen.');
      return;
    }
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.hub.pairWithCode(_conn!, _connHost, _connPort, code, clientName);
    } catch (e) {
      if (mounted) {
        setState(() {
          _conn = null;
          _error = '$e';
        });
      }
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final conn = _conn;
    return ListView(
      children: [
        TextField(
          controller: _host,
          enabled: conn == null && !_busy,
          decoration: const InputDecoration(
            labelText: 'Hub address',
            helperText: 'As shown on the pairing screen, e.g. 192.168.1.20',
          ),
          keyboardType: TextInputType.url,
          autocorrect: false,
        ),
        if (conn == null) ...[
          SizedBox(height: sp.s),
          Wrap(spacing: sp.s, crossAxisAlignment: WrapCrossAlignment.center, children: [
            if (_found == null) const SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2)),
            if (_found != null && _found!.isEmpty) Text('No hub announced itself on this network.', style: theme.textTheme.bodySmall),
            for (final h in _found ?? const <FoundHub>[])
              ActionChip(avatar: const Icon(Icons.router, size: 18), label: Text('${h.name} · $h'), onPressed: () => _host.text = h.toString()),
            if (_found != null) IconButton(onPressed: _search, icon: const Icon(Icons.refresh), tooltip: 'Search again'),
          ]),
        ],
        SizedBox(height: sp.l),
        if (conn == null)
          FilledButton(onPressed: _busy ? null : _connect, child: const Text('Connect'))
        else ...[
          Card(
            child: Padding(
              padding: EdgeInsets.all(sp.l),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('Is this the same as on the hub\'s screen?', style: theme.textTheme.titleSmall),
                  SizedBox(height: sp.s),
                  Text(
                    shortFingerprint(conn.presentedFingerprint),
                    style: theme.textTheme.titleLarge?.copyWith(fontFamily: 'monospace', color: theme.colorScheme.primary),
                  ),
                  SizedBox(height: sp.s),
                  Text('If not, stop: something else answered at this address.', style: theme.textTheme.bodySmall),
                ],
              ),
            ),
          ),
          SizedBox(height: sp.l),
          TextField(
            controller: _code,
            enabled: !_busy,
            autofocus: true,
            keyboardType: TextInputType.number,
            maxLength: 7,
            decoration: const InputDecoration(labelText: 'Code on the hub\'s screen'),
            onSubmitted: (_) => _pair(),
          ),
          FilledButton(onPressed: _busy ? null : _pair, child: const Text('Pair')),
          TextButton(
            onPressed: _busy
                ? null
                : () {
                    conn.close();
                    setState(() => _conn = null);
                  },
            child: const Text('Not the same — cancel'),
          ),
        ],
        if (_error != null) ...[
          SizedBox(height: sp.m),
          Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
        ],
      ],
    );
  }
}
