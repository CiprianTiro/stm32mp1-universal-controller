import 'package:flutter/material.dart';

import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import '../widgets/capability_controls.dart';
import 'wizard_screen.dart';

/// "Add device" (#40): what the hub already found on the network, and
/// every device type it can add -- the same templates the touchscreen
/// offers. Each opens the wizard.
class AddDeviceScreen extends StatefulWidget {
  const AddDeviceScreen({super.key, required this.hub});

  final HubStore hub;

  @override
  State<AddDeviceScreen> createState() => _AddDeviceScreenState();
}

class _AddDeviceScreenState extends State<AddDeviceScreen> {
  List<Map<String, dynamic>>? _templates;
  String? _error;
  bool _searching = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final reply = await widget.hub.request({'action': 'list_templates'});
      if (mounted) setState(() => _templates = (reply['templates'] as List).cast<Map<String, dynamic>>());
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    }
  }

  Future<void> _searchNow() async {
    setState(() => _searching = true);
    // A search round (and a network scan, #73); results arrive as
    // found_changed, which HubStore follows.
    await runRequest(context, () => widget.hub.request({'action': 'discover_now'}, timeout: const Duration(seconds: 60)));
    if (mounted) setState(() => _searching = false);
  }

  void _openWizard(Map<String, dynamic> start, String title) {
    Navigator.of(context).push(MaterialPageRoute(builder: (_) => WizardScreen(hub: widget.hub, start: start, title: title)));
  }

  Future<void> _pickTemplate(Map<String, dynamic> t) async {
    final variants = ((t['variants'] as List?) ?? const []).cast<Map<String, dynamic>>();
    String? variant;
    if (variants.length > 1) {
      variant = await showModalBottomSheet<String>(
        context: context,
        builder: (context) => SafeArea(
          child: Column(mainAxisSize: MainAxisSize.min, children: [
            ListTile(title: Text(t['name'] as String, style: Theme.of(context).textTheme.titleMedium)),
            for (final v in variants)
              ListTile(
                title: Text(v['label'] as String? ?? v['id'] as String),
                trailing: const Icon(Icons.chevron_right),
                onTap: () => Navigator.pop(context, v['id'] as String),
              ),
          ]),
        ),
      );
      if (variant == null) return;
    }
    _openWizard({'action': 'wizard_start', 'template': t['id'], 'variant': ?variant}, t['name'] as String);
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final byCategory = <String, List<Map<String, dynamic>>>{};
    for (final t in _templates ?? const <Map<String, dynamic>>[]) {
      byCategory.putIfAbsent(t['category'] as String? ?? 'Other', () => []).add(t);
    }
    final names = {for (final t in _templates ?? const <Map<String, dynamic>>[]) t['id']: t['name']};

    return Scaffold(
      appBar: AppBar(
        title: const Text('Add device'),
        actions: [
          IconButton(
            tooltip: 'Who is on the network',
            icon: const Icon(Icons.lan_outlined),
            onPressed: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => _HostsScreen(hub: widget.hub))),
          ),
        ],
      ),
      body: ListenableBuilder(
        listenable: widget.hub,
        builder: (context, _) => ListView(
          padding: EdgeInsets.all(sp.l),
          children: [
            Row(children: [
              Expanded(child: Text('Found on your network', style: theme.textTheme.titleMedium)),
              TextButton.icon(
                onPressed: _searching ? null : _searchNow,
                icon: _searching
                    ? const SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2))
                    : const Icon(Icons.search),
                label: const Text('Search now'),
              ),
            ]),
            if (widget.hub.found.isEmpty)
              Padding(
                padding: EdgeInsets.symmetric(vertical: sp.s),
                child: Text('Nothing new found. Devices that announce themselves appear here.', style: theme.textTheme.bodySmall),
              ),
            for (final f in widget.hub.found)
              Padding(
                padding: EdgeInsets.only(bottom: sp.s),
                child: Card(
                  child: ListTile(
                    leading: const Icon(Icons.fiber_new_outlined),
                    title: Text(f['name'] as String? ?? ''),
                    subtitle: Text('${names[f['template']] ?? f['template']} · ${f['address']}'),
                    trailing: const Icon(Icons.add),
                    onTap: () => _openWizard(
                      {'action': 'wizard_start', 'template': f['template'], 'found': f['address']},
                      f['name'] as String? ?? 'Add device',
                    ),
                  ),
                ),
              ),
            SizedBox(height: sp.l),
            Text('Device types', style: theme.textTheme.titleMedium),
            if (_error != null) Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
            if (_templates == null && _error == null)
              Padding(padding: EdgeInsets.all(sp.xl), child: const Center(child: CircularProgressIndicator())),
            for (final cat in byCategory.entries) ...[
              Padding(
                padding: EdgeInsets.only(top: sp.m, bottom: sp.xs),
                child: Text(cat.key, style: theme.textTheme.bodySmall),
              ),
              for (final t in cat.value)
                Padding(
                  padding: EdgeInsets.only(bottom: sp.s),
                  child: Card(
                    child: ListTile(
                      title: Text(t['name'] as String),
                      subtitle: (t['description'] as String? ?? '').isEmpty ? null : Text(t['description'] as String),
                      trailing: const Icon(Icons.chevron_right),
                      onTap: () => _pickTemplate(t),
                    ),
                  ),
                ),
            ],
          ],
        ),
      ),
    );
  }
}

/// Who is on the network (#73): every host the hub sees, with the maker
/// from its MAC address -- to recognise a device that announces nothing.
class _HostsScreen extends StatefulWidget {
  const _HostsScreen({required this.hub});
  final HubStore hub;

  @override
  State<_HostsScreen> createState() => _HostsScreenState();
}

class _HostsScreenState extends State<_HostsScreen> {
  List<Map<String, dynamic>>? _hosts;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() => _error = null);
    try {
      final reply = await widget.hub.request({'action': 'network_hosts'});
      if (mounted) setState(() => _hosts = (reply['hosts'] as List).cast<Map<String, dynamic>>());
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('Who is on the network'), actions: [
        IconButton(onPressed: _load, icon: const Icon(Icons.refresh)),
      ]),
      body: _error != null
          ? Center(child: Text(_error!))
          : _hosts == null
              ? const Center(child: CircularProgressIndicator())
              : ListView(
                  padding: EdgeInsets.all(sp.l),
                  children: [
                    Text('Every device the hub has seen on its network. A search (Add device → Search now) '
                        'refreshes the list.', style: Theme.of(context).textTheme.bodySmall),
                    SizedBox(height: sp.m),
                    for (final h in _hosts!)
                      ListTile(
                        leading: const Icon(Icons.devices_other),
                        title: Text((h['manufacturer'] as String? ?? '').isEmpty ? 'Unknown maker' : h['manufacturer'] as String),
                        subtitle: Text('${h['address']} · ${h['mac']} · ${h['interface']}'),
                      ),
                  ],
                ),
    );
  }
}
