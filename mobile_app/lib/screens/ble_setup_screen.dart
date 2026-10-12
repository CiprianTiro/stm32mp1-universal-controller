import 'dart:io';

import 'package:flutter/material.dart';

import '../ble/ble_setup.dart';
import '../hub/credentials.dart';
import '../hub/hub_address.dart';
import '../hub/hub_discovery.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';

/// First-time setup of a hub that isn't on any network yet (#36): over
/// Bluetooth, the phone gives it the home WiFi and is paired in the same
/// step. Everything is asked BEFORE searching (as tools/ble_setup.py does):
/// a phone may forget a found device again while the person types.
class BleSetupScreen extends StatefulWidget {
  const BleSetupScreen({super.key, required this.hub});

  final HubStore hub;

  @override
  State<BleSetupScreen> createState() => _BleSetupScreenState();
}

class _BleSetupScreenState extends State<BleSetupScreen> {
  final _code = TextEditingController();
  final _ssid = TextEditingController();
  final _password = TextEditingController();
  bool _showPassword = false;
  bool _busy = false;
  String? _progress;
  String? _error;
  String? _wifiProblem;

  @override
  void dispose() {
    _code.dispose();
    _ssid.dispose();
    _password.dispose();
    super.dispose();
  }

  void _step(String text) {
    if (mounted) setState(() => _progress = text);
  }

  Future<void> _run() async {
    final code = _code.text.replaceAll(RegExp(r'\s'), '');
    if (!RegExp(r'^\d{6}$').hasMatch(code)) {
      setState(() => _error = 'The code is the 6 digits on the hub\'s screen.');
      return;
    }
    if (_ssid.text.trim().isEmpty) {
      setState(() => _error = 'Which WiFi should the hub join?');
      return;
    }
    setState(() {
      _busy = true;
      _error = null;
      _wifiProblem = null;
    });
    final bt = HubBluetooth();
    try {
      await bt.ensurePermissions();
      _step('Looking for the hub over Bluetooth…');
      final link = await bt.connect();
      final protocol = BleSetupProtocol(link);
      _step('Checking the code…');
      await protocol.exchangeKeys(code);
      _step('Sending the WiFi details (encrypted)…');
      final paired = await protocol.sendWifi(
        ssid: _ssid.text.trim(),
        password: _password.text,
        clientName: 'Phone app (${Platform.operatingSystem})',
      );
      _step('The hub is joining ${_ssid.text.trim()}…');
      String? wifiProblem;
      try {
        wifiProblem = await protocol.wifiResult();
      } on BleSetupException catch (e) {
        wifiProblem = e.message;
      }
      await bt.disconnect();

      // Paired either way (the hub may also be on a cable): find where it
      // is on the network now. Its certificate is pinned from here on.
      _step('Finding the hub on the network…');
      var host = defaultHubHost, port = lanPort;
      for (var i = 0; i < 5; i++) {
        final hubs = await findHubs(within: const Duration(seconds: 3));
        if (hubs.isNotEmpty) {
          host = hubs.first.host;
          port = hubs.first.port;
          break;
        }
      }
      await widget.hub.adoptCredentials(HubCredentials(
        host: host,
        port: port,
        clientId: paired.clientId,
        token: paired.token,
        fingerprint: paired.hubFingerprint,
      ));
      if (!mounted) return;
      if (wifiProblem == null) {
        Navigator.of(context).popUntil((r) => r.isFirst);
      } else {
        setState(() => _wifiProblem = wifiProblem);
      }
    } on BleSetupException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    } finally {
      await bt.disconnect();
      if (mounted) {
        setState(() {
          _busy = false;
          _progress = null;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('Set up a new hub')),
      body: SafeArea(
        child: ListView(
          padding: EdgeInsets.all(sp.xl),
          children: [
            Text(
              'For a hub that isn\'t on your network yet. On its screen open Settings → Paired devices → '
              'Pair a new device: the hub is visible over Bluetooth while that code is shown.',
              style: theme.textTheme.bodyLarge,
            ),
            SizedBox(height: sp.xl),
            TextField(
              controller: _code,
              enabled: !_busy,
              keyboardType: TextInputType.number,
              maxLength: 7,
              decoration: const InputDecoration(labelText: 'Code on the hub\'s screen'),
            ),
            TextField(
              controller: _ssid,
              enabled: !_busy,
              // Exactly what's typed: no suggestions, no capital first
              // letter, no curly quotes or long dashes (iOS smart
              // punctuation) -- a WiFi name and password are taken byte
              // for byte by the router.
              autocorrect: false,
              enableSuggestions: false,
              smartDashesType: SmartDashesType.disabled,
              smartQuotesType: SmartQuotesType.disabled,
              decoration: const InputDecoration(labelText: 'WiFi name', helperText: 'The network the hub should join'),
            ),
            SizedBox(height: sp.l),
            TextField(
              controller: _password,
              enabled: !_busy,
              obscureText: !_showPassword,
              // Also while shown (no longer a secret field for iOS): see
              // the WiFi name above.
              autocorrect: false,
              enableSuggestions: false,
              smartDashesType: SmartDashesType.disabled,
              smartQuotesType: SmartQuotesType.disabled,
              onChanged: (_) => setState(() {}),
              decoration: InputDecoration(
                labelText: 'WiFi password',
                helperText: 'Empty for an open network. Sent encrypted, only to the hub.',
                // A space at either end is allowed in WiFi passwords, but
                // is almost always a slip (iOS adds one after AutoFill).
                errorText: _password.text != _password.text.trim() ? 'It starts or ends with a space: is that right?' : null,
                suffixIcon: IconButton(
                  icon: Icon(_showPassword ? Icons.visibility_off : Icons.visibility),
                  onPressed: () => setState(() => _showPassword = !_showPassword),
                ),
              ),
            ),
            SizedBox(height: sp.xl),
            FilledButton.icon(
              onPressed: _busy ? null : _run,
              icon: const Icon(Icons.bluetooth_searching),
              label: const Text('Set up'),
            ),
            if (_progress != null) ...[
              SizedBox(height: sp.l),
              Row(children: [
                const SizedBox(width: 18, height: 18, child: CircularProgressIndicator(strokeWidth: 2)),
                SizedBox(width: sp.m),
                Expanded(child: Text(_progress!)),
              ]),
            ],
            if (_error != null) ...[
              SizedBox(height: sp.l),
              Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
            ],
            if (_wifiProblem != null) ...[
              SizedBox(height: sp.l),
              Card(
                child: Padding(
                  padding: EdgeInsets.all(sp.l),
                  child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                    Text('Paired, but the hub could not join the WiFi', style: theme.textTheme.titleSmall),
                    SizedBox(height: sp.s),
                    Text(_wifiProblem!),
                    SizedBox(height: sp.s),
                    Text('Check the name and password, show a new code on the hub and run the setup again — '
                        'or connect the hub with a cable.', style: theme.textTheme.bodySmall),
                    SizedBox(height: sp.m),
                    OutlinedButton(
                      onPressed: () => Navigator.of(context).popUntil((r) => r.isFirst),
                      child: const Text('Continue'),
                    ),
                  ]),
                ),
              ),
            ],
          ],
        ),
      ),
    );
  }
}
