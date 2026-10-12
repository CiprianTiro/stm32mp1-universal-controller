import 'package:flutter/material.dart';

import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import '../widgets/capability_controls.dart';

/// The hub's look (#39), its place in the world (#47) and the vendor
/// accounts it's signed in to (#74): the same settings as on the hub's
/// screen. A change shows on the hub, on this
/// phone and on every other phone at once (settings_changed).
class HubSettingsScreen extends StatefulWidget {
  const HubSettingsScreen({super.key, required this.hub});
  final HubStore hub;

  @override
  State<HubSettingsScreen> createState() => _HubSettingsScreenState();
}

class _HubSettingsScreenState extends State<HubSettingsScreen> {
  late final _timeZone = TextEditingController(text: widget.hub.settings['time_zone'] as String? ?? '');
  late final _lat = TextEditingController(text: '${widget.hub.settings['latitude'] ?? ''}');
  late final _lon = TextEditingController(text: '${widget.hub.settings['longitude'] ?? ''}');

  static const _accents = {
    'sky': Color(0xFF38BDF8),
    'emerald': Color(0xFF34D399),
    'amber': Color(0xFFFBBF24),
    'violet': Color(0xFFA78BFA),
    'rose': Color(0xFFFB7185),
  };

  @override
  void dispose() {
    _timeZone.dispose();
    _lat.dispose();
    _lon.dispose();
    super.dispose();
  }

  /// The vendor accounts (#74, accounts.rs): null until loaded.
  List<Map<String, dynamic>>? _accounts;

  @override
  void initState() {
    super.initState();
    _loadAccounts();
  }

  void _takeAccounts(Map<String, dynamic> reply) {
    if (mounted) setState(() => _accounts = ((reply['accounts'] as List?) ?? const []).cast<Map<String, dynamic>>());
  }

  Future<void> _loadAccounts() async {
    try {
      _takeAccounts(await widget.hub.request({'action': 'list_accounts'}));
    } catch (_) {
      if (mounted) setState(() => _accounts = const []);
    }
  }

  Future<void> _signOut(Map<String, dynamic> a) async {
    final devices = ((a['devices'] as List?) ?? const []).cast<String>();
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Sign out of ${a['account']}?'),
        content: Text(devices.isEmpty
            ? 'The hub forgets this account\'s session.'
            : 'These stop working until you sign in again (“Pair again” on each): ${devices.join(', ')}.'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Sign out')),
        ],
      ),
    );
    if (ok == true && mounted) {
      await runRequest(context, () async => _takeAccounts(await widget.hub.request({'action': 'remove_account', 'id': a['id']})));
    }
  }

  void _set(Map<String, dynamic> values) => runRequest(context, () => widget.hub.setSettings(values));

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    return ListenableBuilder(
      listenable: widget.hub,
      builder: (context, _) {
        final s = widget.hub.settings;
        return Scaffold(
          appBar: AppBar(title: const Text('Hub settings')),
          body: ListView(padding: EdgeInsets.all(sp.l), children: [
            Text('Look', style: theme.textTheme.titleMedium),
            Text('For the hub\'s screen and this app.', style: theme.textTheme.bodySmall),
            SizedBox(height: sp.m),
            SegmentedButton<String>(
              segments: const [
                ButtonSegment(value: 'dark', label: Text('Dark')),
                ButtonSegment(value: 'light', label: Text('Light')),
                ButtonSegment(value: 'auto', label: Text('Auto')),
              ],
              selected: {s['mode'] as String? ?? 'dark'},
              onSelectionChanged: (v) => _set({'mode': v.first}),
            ),
            SizedBox(height: sp.m),
            Wrap(spacing: sp.s, children: [
              for (final a in _accents.entries)
                ChoiceChip(
                  avatar: CircleAvatar(backgroundColor: a.value),
                  label: Text(a.key[0].toUpperCase() + a.key.substring(1)),
                  selected: s['accent'] == a.key,
                  onSelected: (_) => _set({'accent': a.key}),
                ),
            ]),
            SizedBox(height: sp.m),
            SegmentedButton<String>(
              segments: const [
                ButtonSegment(value: 'comfortable', label: Text('Comfortable')),
                ButtonSegment(value: 'compact', label: Text('Compact')),
              ],
              selected: {s['density'] as String? ?? 'comfortable'},
              onSelectionChanged: (v) => _set({'density': v.first}),
            ),
            SizedBox(height: sp.xl),
            Text('Time and place', style: theme.textTheme.titleMedium),
            Text('Automations run on the hub\'s clock; sunrise and sunset need its location.', style: theme.textTheme.bodySmall),
            SizedBox(height: sp.m),
            TextField(
              controller: _timeZone,
              autocorrect: false,
              decoration: const InputDecoration(labelText: 'Time zone', helperText: 'An IANA name, e.g. Europe/Bucharest'),
              onSubmitted: (v) => _set({'time_zone': v.trim()}),
            ),
            SizedBox(height: sp.m),
            Row(children: [
              Expanded(
                child: TextField(
                  controller: _lat,
                  keyboardType: const TextInputType.numberWithOptions(decimal: true, signed: true),
                  decoration: const InputDecoration(labelText: 'Latitude', helperText: 'North +'),
                ),
              ),
              SizedBox(width: sp.m),
              Expanded(
                child: TextField(
                  controller: _lon,
                  keyboardType: const TextInputType.numberWithOptions(decimal: true, signed: true),
                  decoration: const InputDecoration(labelText: 'Longitude', helperText: 'East +'),
                ),
              ),
            ]),
            SizedBox(height: sp.m),
            Align(
              alignment: Alignment.centerRight,
              child: FilledButton(
                onPressed: () {
                  final lat = double.tryParse(_lat.text.trim().replaceAll(',', '.'));
                  final lon = double.tryParse(_lon.text.trim().replaceAll(',', '.'));
                  _set({
                    'time_zone': _timeZone.text.trim(),
                    'latitude': ?lat,
                    'longitude': ?lon,
                  });
                },
                child: const Text('Save time and place'),
              ),
            ),
            SizedBox(height: sp.xl),
            Text('Vendor accounts', style: theme.textTheme.titleMedium),
            Text('Cloud accounts the hub is signed in to (it keeps their session, never the password). '
                'Added when you add a device that needs one.', style: theme.textTheme.bodySmall),
            SizedBox(height: sp.s),
            if (_accounts == null)
              const Center(child: CircularProgressIndicator())
            else if (_accounts!.isEmpty)
              Text('None.', style: theme.textTheme.bodySmall)
            else
              for (final a in _accounts!)
                Card(
                  child: ListTile(
                    leading: const Icon(Icons.cloud_outlined),
                    title: Text('${a['account']}'),
                    subtitle: Text([
                      _vendorName(a['vendor'] as String? ?? ''),
                      if (((a['devices'] as List?) ?? const []).isNotEmpty) (a['devices'] as List).join(', '),
                    ].join(' · ')),
                    trailing: TextButton(onPressed: () => _signOut(a), child: const Text('Sign out')),
                  ),
                ),
          ]),
        );
      },
    );
  }
}

/// accounts.rs's vendor ids, as people know them.
String _vendorName(String vendor) => switch (vendor) {
      'roborock' => 'Roborock',
      'ezviz' => 'EZVIZ',
      'tapo' => 'Tapo',
      _ => vendor,
    };
