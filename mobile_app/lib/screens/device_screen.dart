import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../hub/device.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import '../widgets/capability_controls.dart';
import 'home_screen.dart';
import '../widgets/hero_controls.dart';
import '../widgets/power_link.dart';
import 'wizard_screen.dart';

/// One device: all its controls, its favourite star (#95), and renaming /
/// moving it to another room. Follows the hub live; closes by itself if the
/// device is removed (on the hub, or by another phone).
class DeviceScreen extends StatefulWidget {
  const DeviceScreen({super.key, required this.hub, required this.id});

  final HubStore hub;
  final String id;

  @override
  State<DeviceScreen> createState() => _DeviceScreenState();
}

class _DeviceScreenState extends State<DeviceScreen> {
  HubStore get hub => widget.hub;
  String get id => widget.id;

  @override
  void initState() {
    super.initState();
    // To know whether "pair again" / "change settings" exist for its type.
    hub.loadTemplates().catchError((_) {});
  }

  void _wizard(String action, Device d, String title) => Navigator.of(context).push(MaterialPageRoute(
      builder: (_) => WizardScreen(hub: hub, start: {'action': action, 'device': d.id}, title: title)));

  Future<void> _webhook(Device d) async {
    final messenger = ScaffoldMessenger.of(context);
    try {
      final url = await hub.webhookUrl(d.id);
      if (!mounted) return;
      await showDialog<void>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Webhook address'),
          content: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
            const Text('Calling this address makes the hub read the device\'s state at once (e.g. a Shelly\'s '
                'action on switching). Keep it secret: it works without a password.'),
            const SizedBox(height: 12),
            SelectableText(url, style: const TextStyle(fontFamily: 'monospace')),
          ]),
          actions: [
            TextButton(
              onPressed: () {
                Clipboard.setData(ClipboardData(text: url));
                Navigator.pop(context);
              },
              child: const Text('Copy'),
            ),
            TextButton(onPressed: () => Navigator.pop(context), child: const Text('Close')),
          ],
        ),
      );
    } catch (e) {
      messenger.showSnackBar(SnackBar(content: Text('$e')));
    }
  }

  Future<void> _remove(Device d) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Remove ${d.name}?'),
        content: const Text('It disappears from the hub, its scenes and the cloud. To use it again, add it again.'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Remove')),
        ],
      ),
    );
    if (ok == true && mounted) await runRequest(context, () => hub.removeDevice(d.id));
  }

  Future<void> _edit(BuildContext context, Device d) async {
    final name = TextEditingController(text: d.name);
    final room = TextEditingController(text: d.room);
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Name and room'),
        content: Column(mainAxisSize: MainAxisSize.min, children: [
          TextField(controller: name, decoration: const InputDecoration(labelText: 'Name'), autofocus: true),
          const SizedBox(height: 12),
          Autocomplete<String>(
            initialValue: TextEditingValue(text: d.room),
            optionsBuilder: (v) => hub.rooms.where((r) => r.isNotEmpty && r.toLowerCase().contains(v.text.toLowerCase())),
            onSelected: (r) => room.text = r,
            fieldViewBuilder: (context, controller, focus, _) => TextField(
              controller: controller,
              focusNode: focus,
              decoration: const InputDecoration(labelText: 'Room'),
              onChanged: (t) => room.text = t,
            ),
          ),
        ]),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Save')),
        ],
      ),
    );
    if (ok == true && context.mounted) {
      await runRequest(context, () => hub.updateInfo(d.id, name: name.text.trim(), room: room.text.trim()));
    }
    name.dispose();
    room.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: hub,
      builder: (context, _) {
        final d = hub.device(id);
        if (d == null) {
          WidgetsBinding.instance.addPostFrameCallback((_) {
            if (context.mounted) Navigator.of(context).maybePop();
          });
          return const Scaffold();
        }
        final sp = Spacing.of(context);
        final theme = Theme.of(context);
        return Scaffold(
          extendBodyBehindAppBar: true,
          appBar: AppBar(
            backgroundColor: Colors.transparent,
            actions: [
              IconButton(
                tooltip: d.favourite ? 'Remove from favourites' : 'Add to favourites',
                icon: Icon(d.favourite ? Icons.star_rounded : Icons.star_border_rounded),
                onPressed: () => runRequest(context, () => hub.updateInfo(d.id, favourite: !d.favourite)),
              ),
              IconButton(tooltip: 'Name and room', icon: const Icon(Icons.edit_outlined), onPressed: () => _edit(context, d)),
              PopupMenuButton<String>(
                onSelected: (v) => switch (v) {
                  'reauth' => _wizard('wizard_reauth', d, 'Pair again'),
                  'reconfigure' => _wizard('wizard_reconfigure', d, 'Change settings'),
                  'webhook' => _webhook(d),
                  'remove' => _remove(d),
                  _ => null,
                },
                itemBuilder: (context) {
                  final t = hub.templateOf(d);
                  return [
                    if (t?['can_reauth'] == true) const PopupMenuItem(value: 'reauth', child: Text('Pair again…')),
                    if (t?['can_reconfigure'] == true) const PopupMenuItem(value: 'reconfigure', child: Text('Change settings…')),
                    if (d.source != 'virtual') const PopupMenuItem(value: 'webhook', child: Text('Webhook address…')),
                    const PopupMenuItem(value: 'remove', child: Text('Remove device…')),
                  ];
                },
              ),
            ],
          ),
          body: Builder(builder: (context) {
            final hero = DeviceHero.of(d, hub);
            final active = isActive(d) && d.reachable;
            // A soft glow of the accent behind the page while the device is
            // on -- the room "lit" by it.
            return AnimatedContainer(
              duration: const Duration(milliseconds: 400),
              decoration: BoxDecoration(
                gradient: RadialGradient(
                  center: const Alignment(0, -0.75),
                  radius: 1.1,
                  colors: [
                    theme.colorScheme.primary.withValues(alpha: active ? 0.22 : 0.0),
                    theme.scaffoldBackgroundColor.withValues(alpha: 0),
                  ],
                ),
              ),
              child: ListView(
                padding: EdgeInsets.fromLTRB(sp.xl, MediaQuery.of(context).padding.top + kToolbarHeight, sp.xl, sp.xl * 2),
                children: [
                  Text(d.name, style: theme.textTheme.headlineMedium),
                  SizedBox(height: sp.xs),
                  Text(
                    [if (d.room.isNotEmpty) d.room, summaryOf(d)].where((s) => s.isNotEmpty).join(' · '),
                    style: theme.textTheme.bodySmall,
                  ),
                  if (d.online == 'unauthorized' && hub.templateOf(d)?['can_reauth'] == true) ...[
                    SizedBox(height: sp.m),
                    FilledButton.icon(
                      onPressed: () => _wizard('wizard_reauth', d, 'Pair again'),
                      icon: const Icon(Icons.link),
                      label: const Text('Pair again'),
                    ),
                  ],
                  IgnorePointer(
                    ignoring: !d.reachable,
                    child: Opacity(
                      opacity: d.reachable ? 1 : 0.5,
                      child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
                        if (hero.widget != null) Padding(padding: EdgeInsets.symmetric(vertical: sp.xl * 2), child: Center(child: hero.widget)),
                        if (hero.widget == null) SizedBox(height: sp.xl),
                        CapabilityControls(device: d, hub: hub, skip: hero.handles),
                      ]),
                    ),
                  ),
                  // Set up also while the device is unreachable: it may be
                  // unreachable BECAUSE its plug is off.
                  if (PowerLinkSection.applies(d)) PowerLinkSection(device: d, hub: hub),
                ],
              ),
            );
          }),
        );
      },
    );
  }
}
