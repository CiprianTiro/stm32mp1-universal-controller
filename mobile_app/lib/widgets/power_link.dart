import 'package:flutter/material.dart';

import '../hub/device.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import 'capability_controls.dart';

/// "Powered by" (#99, backend power_link.rs): an IR device -- it can't say
/// what it's doing -- paired with the plug that powers it. Switching the
/// device then also switches the plug (and, with "cut power", off cuts it);
/// with a plug that measures, its on/off follows the measured power once
/// learned. The link lives in the device's config: powered_by, cut_power,
/// start_delay_s, power_off_w / power_on_w / power_threshold_w,
/// power_problem.
class PowerLinkSection extends StatefulWidget {
  const PowerLinkSection({super.key, required this.device, required this.hub});

  final Device device;
  final HubStore hub;

  /// Only IR blaster devices can be linked (power_link.rs's IR_ADAPTER).
  static bool applies(Device d) => d.source == 'ir-blaster';

  @override
  State<PowerLinkSection> createState() => _PowerLinkSectionState();
}

class _PowerLinkSectionState extends State<PowerLinkSection> {
  /// What the last learning step found, in words ("" = nothing yet).
  String _learning = '';
  bool _busy = false;

  Device get d => widget.device;
  HubStore get hub => widget.hub;

  /// The plugs it can be linked to: anything switchable that isn't an IR
  /// device or itself linked, and is a plug (it measures, or its type is
  /// one) -- the same list as the hub's screen.
  List<Device> _plugs() {
    final plugs = hub.devices.where((p) {
      if (p.id == d.id || !p.has('switch') || p.source == 'ir-blaster' || p.config.containsKey('powered_by')) return false;
      return p.has('energy') || hub.templateOf(p)?['category'] == 'plugs';
    }).toList();
    plugs.sort((a, b) => a.name.compareTo(b.name));
    return plugs;
  }

  Future<void> _set({String? plug, bool? cut, int? delay}) async {
    final c = d.config;
    final current = c['powered_by'] ?? '';
    await runRequest(context, () async {
      await hub.request({
        'action': 'set_power_link',
        'id': d.id,
        'plug': plug ?? current,
        'cut_power': cut ?? c['cut_power'] == 'on',
        'start_delay_s': delay ?? int.tryParse(c['start_delay_s'] ?? ''),
      });
    });
  }

  Future<void> _learn(String step) async {
    setState(() {
      _busy = true;
      _learning = '';
    });
    try {
      // The hub switches the device and waits for the plug's reading.
      final reply = await hub.request({'action': 'learn_power', 'id': d.id, 'step': step}, timeout: const Duration(seconds: 90));
      final r = reply['result'];
      if (!mounted) return;
      setState(() => _learning = switch (step) {
            'off' => 'Off: ${r is Map ? r['watts'] : '?'} W. Now step 2.',
            'on' => 'On: ${r is Map ? r['watts'] : '?'} W. Learned: from ${r is Map ? r['threshold'] : '?'} W it counts as on.',
            _ => 'Forgotten: its state is assumed again.',
          });
    } catch (e) {
      if (mounted) setState(() => _learning = 'Not learned: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final c = d.config;
    final plugId = c['powered_by'] ?? '';
    final plug = hub.device(plugId);
    final plugs = _plugs();
    final measures = plug?.has('energy') ?? false;
    final learned = c.containsKey('power_threshold_w');
    final delay = int.tryParse(c['start_delay_s'] ?? '') ?? 2;
    final status = plugId.isEmpty
        ? 'Not plugged into a plug the hub knows'
        : (c['power_problem'] ?? (learned ? 'State confirmed by the plug' : 'State assumed'));

    return Card(
      child: Padding(
        padding: EdgeInsets.all(sp.l),
        child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text('Powered by', style: theme.textTheme.titleSmall),
          SizedBox(height: sp.xs),
          Text(plug == null ? status : '${plug.name} · $status', style: theme.textTheme.bodySmall),
          SizedBox(height: sp.m),
          DropdownButtonFormField<String>(
            initialValue: plugId,
            isExpanded: true,
            decoration: const InputDecoration(labelText: 'Plug'),
            items: [
              const DropdownMenuItem(value: '', child: Text('None')),
              // The linked plug, also when it no longer fits the list.
              if (plug != null && !plugs.any((p) => p.id == plug.id)) DropdownMenuItem(value: plug.id, child: Text(plug.name)),
              for (final p in plugs)
                DropdownMenuItem(value: p.id, child: Text(p.has('energy') ? '${p.name} · measures power' : p.name)),
            ],
            onChanged: (v) => _set(plug: v ?? ''),
          ),
          if (plugId.isNotEmpty) ...[
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Off also cuts the power'),
              subtitle: const Text('No standby draw; the plug comes back on with the device.'),
              value: c['cut_power'] == 'on',
              onChanged: (v) => _set(cut: v),
            ),
            Row(children: [
              Expanded(child: Text('Ready after power-on: $delay s')),
              IconButton(onPressed: delay > 0 ? () => _set(delay: delay - 1) : null, icon: const Icon(Icons.remove)),
              IconButton(onPressed: delay < 60 ? () => _set(delay: delay + 1) : null, icon: const Icon(Icons.add)),
            ]),
            if (measures) ...[
              SizedBox(height: sp.s),
              Text('Confirmed state', style: theme.textTheme.titleSmall),
              SizedBox(height: sp.xs),
              Text(
                learned
                    ? 'Off ${c['power_off_w']} W · on ${c['power_on_w']} W: its state follows the plug\'s measurement.'
                    : 'Teach the hub what it draws: 1. off, 2. on. Then its state follows the plug, '
                        'also when the original remote is used.',
                style: theme.textTheme.bodySmall,
              ),
              SizedBox(height: sp.s),
              Wrap(spacing: sp.s, runSpacing: sp.s, children: [
                OutlinedButton(onPressed: _busy ? null : () => _learn('off'), child: const Text('1. Learn off')),
                OutlinedButton(onPressed: _busy ? null : () => _learn('on'), child: const Text('2. Learn on')),
                if (learned) TextButton(onPressed: _busy ? null : () => _learn('forget'), child: const Text('Forget')),
              ]),
              if (_busy) Padding(padding: EdgeInsets.only(top: sp.s), child: const LinearProgressIndicator()),
              if (_learning.isNotEmpty) Padding(padding: EdgeInsets.only(top: sp.s), child: Text(_learning)),
            ],
          ],
        ]),
      ),
    );
  }
}
