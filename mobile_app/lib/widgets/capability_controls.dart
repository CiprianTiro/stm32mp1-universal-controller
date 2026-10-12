import 'dart:convert';

import 'package:flutter/material.dart';

import '../hub/device.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';

/// A device's controls, built from its capabilities alone (protocol v2):
/// whatever device it is, a switch is a toggle, a dimmer a slider, a cover
/// open/stop/close. The order is the touchscreen's: the main control first,
/// then settings, then what is only read.
///
/// Nothing is changed on screen before the hub confirms it: every request
/// answers with the device as the HARDWARE reports it, and a refused one
/// shows the hub's reason.
class CapabilityControls extends StatelessWidget {
  const CapabilityControls({super.key, required this.device, required this.hub, this.skip = const {}});

  final Device device;
  final HubStore hub;

  /// Capabilities shown elsewhere (the page's hero control).
  final Set<String> skip;

  static const _order = ['switch', 'dimmer', 'color', 'cover', 'climate', 'lock', 'media', 'remote', 'sensor', 'energy'];

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final sections = <Widget>[
      for (final name in _order)
        if (device.has(name) && !skip.contains(name)) _section(name),
      for (final name in device.capabilities.keys)
        if (!knownCapabilities.contains(name)) _UnknownCapability(name: name, state: device.cap(name)!),
    ];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final s in sections) ...[s, SizedBox(height: sp.l)],
      ],
    );
  }

  Widget _section(String name) {
    final d = device;
    return switch (name) {
      'switch' => _SwitchControl(device: d, hub: hub),
      'dimmer' => _DimmerControl(device: d, hub: hub),
      'color' => _ColorControl(device: d, hub: hub),
      'cover' => _CoverControl(device: d, hub: hub),
      'climate' => _ClimateControl(device: d, hub: hub),
      'lock' => _LockControl(device: d, hub: hub),
      'media' => _MediaControl(device: d, hub: hub),
      'remote' => _RemoteControl(device: d, hub: hub),
      'sensor' => _SensorReadings(device: d),
      'energy' => _EnergyReadings(device: d),
      _ => const SizedBox.shrink(),
    };
  }
}

/// Runs a request; a refusal or a lost connection becomes a snackbar.
Future<void> runRequest(BuildContext context, Future<void> Function() request) async {
  final messenger = ScaffoldMessenger.of(context);
  try {
    await request();
  } catch (e) {
    messenger.showSnackBar(SnackBar(content: Text('$e')));
  }
}

class _Section extends StatelessWidget {
  const _Section({required this.title, required this.child, this.trailing});

  final String title;
  final Widget child;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Card(
      child: Padding(
        padding: EdgeInsets.all(sp.l),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(children: [
              Expanded(child: Text(title, style: Theme.of(context).textTheme.titleSmall)),
              ?trailing,
            ]),
            SizedBox(height: sp.s),
            child,
          ],
        ),
      ),
    );
  }
}

// ---- switch ----

class _SwitchControl extends StatelessWidget {
  const _SwitchControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final on = device.isOn ?? false;
    return Card(
      child: SwitchListTile(
        title: Text(on ? 'On' : 'Off', style: Theme.of(context).textTheme.titleSmall),
        value: on,
        onChanged: (v) => runRequest(context, () => hub.command(device.id, 'switch', {'on': v})),
      ),
    );
  }
}

// ---- dimmer ----

/// A slider that follows the finger locally and sends once, on release:
/// a request per pixel of drag would flood the device.
class _LevelSlider extends StatefulWidget {
  const _LevelSlider({required this.value, required this.onCommit, this.divisions, this.label});

  /// 0..100, the range of every level in the protocol (dimmer, cover
  /// position, volume).
  static const double min = 0, max = 100;

  final double value;
  final int? divisions;
  final String Function(double)? label;
  final ValueChanged<double> onCommit;

  @override
  State<_LevelSlider> createState() => _LevelSliderState();
}

class _LevelSliderState extends State<_LevelSlider> {
  double? _dragging;

  @override
  Widget build(BuildContext context) {
    final v = (_dragging ?? widget.value).clamp(_LevelSlider.min, _LevelSlider.max);
    return Slider(
      value: v,
      min: _LevelSlider.min,
      max: _LevelSlider.max,
      divisions: widget.divisions,
      label: widget.label?.call(v) ?? '${v.round()} %',
      onChanged: (x) => setState(() => _dragging = x),
      onChangeEnd: (x) {
        setState(() => _dragging = null);
        widget.onCommit(x);
      },
    );
  }
}

class _DimmerControl extends StatelessWidget {
  const _DimmerControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final level = device.level ?? 0;
    return _Section(
      title: 'Brightness',
      trailing: Text('$level %'),
      child: _LevelSlider(
        value: level.toDouble(),
        divisions: 100,
        onCommit: (v) => runRequest(context, () => hub.command(device.id, 'dimmer', {'level': v.round()})),
      ),
    );
  }
}

// ---- color ----

class _ColorControl extends StatelessWidget {
  const _ColorControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  /// Offered when the device has no palette of its own (a full-colour bulb
  /// or WLED strip takes any colour).
  static const _presets = ['#FF0000', '#FF8800', '#FFDD00', '#00FF00', '#00FFFF', '#0000FF', '#8800FF', '#FF00AA'];
  static const _whites = {2700: 'Warm', 4000: 'Neutral', 6500: 'Daylight'};

  @override
  Widget build(BuildContext context) {
    final c = device.cap('color')!;
    final hex = (c['hex'] as String?)?.toUpperCase();
    final kelvin = (c['kelvin'] as num?)?.toInt();
    final palette = (c['palette'] as List?)?.cast<String>() ?? const [];
    final sp = Spacing.of(context);

    void send(Map<String, dynamic> v) => runRequest(context, () => hub.command(device.id, 'color', v));

    return _Section(
      title: 'Colour',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Wrap(
            spacing: sp.s,
            runSpacing: sp.s,
            children: [
              for (final h in palette.isNotEmpty ? palette : _presets)
                _Swatch(color: _parseHex(h), selected: h.toUpperCase() == hex, onTap: () => send({'hex': h.toUpperCase()})),
            ],
          ),
          // A light with a fixed palette (an IR strip, #85) has no whites.
          if (palette.isEmpty) ...[
            SizedBox(height: sp.m),
            Wrap(
              spacing: sp.s,
              children: [
                for (final w in _whites.entries)
                  ChoiceChip(
                    label: Text(w.value),
                    selected: kelvin == w.key,
                    onSelected: (_) => send({'kelvin': w.key}),
                  ),
              ],
            ),
          ],
        ],
      ),
    );
  }
}

Color _parseHex(String hex) {
  final h = hex.replaceFirst('#', '');
  return Color(int.tryParse('FF$h', radix: 16) ?? 0xFF888888);
}

class _Swatch extends StatelessWidget {
  const _Swatch({required this.color, required this.selected, required this.onTap});
  final Color color;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return InkWell(
      onTap: onTap,
      customBorder: const CircleBorder(),
      child: Container(
        width: 40,
        height: 40,
        decoration: BoxDecoration(
          color: color,
          shape: BoxShape.circle,
          border: Border.all(color: selected ? scheme.onSurface : scheme.outline, width: selected ? 3 : 1),
        ),
      ),
    );
  }
}

// ---- cover ----

class _CoverControl extends StatelessWidget {
  const _CoverControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final c = device.cap('cover')!;
    final position = (c['position'] as num?)?.toInt();
    final moving = c['moving'] as String? ?? 'stopped';
    final canPosition = c['can_position'] as bool? ?? false;
    final sp = Spacing.of(context);

    void act(String name) => runRequest(context, () => hub.action(device.id, 'cover', name));

    final status = switch (moving) {
      'opening' => 'Opening…',
      'closing' => 'Closing…',
      _ => position == null ? '' : (position == 0 ? 'Closed' : (position == 100 ? 'Open' : '$position % open')),
    };
    return _Section(
      title: 'Cover',
      trailing: Text(status),
      child: Column(
        children: [
          if (canPosition && position != null)
            _LevelSlider(
              value: position.toDouble(),
              divisions: 100,
              label: (v) => '${v.round()} % open',
              onCommit: (v) => runRequest(context, () => hub.command(device.id, 'cover', {'position': v.round()})),
            ),
          Row(
            children: [
              Expanded(child: OutlinedButton.icon(onPressed: () => act('close'), icon: const Icon(Icons.arrow_downward), label: const Text('Close'))),
              SizedBox(width: sp.s),
              Expanded(child: OutlinedButton.icon(onPressed: () => act('stop'), icon: const Icon(Icons.stop), label: const Text('Stop'))),
              SizedBox(width: sp.s),
              Expanded(child: OutlinedButton.icon(onPressed: () => act('open'), icon: const Icon(Icons.arrow_upward), label: const Text('Open'))),
            ],
          ),
        ],
      ),
    );
  }
}

// ---- climate ----

class _ClimateControl extends StatelessWidget {
  const _ClimateControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  static const _modeLabels = {'off': 'Off', 'heat': 'Heat', 'cool': 'Cool', 'auto': 'Auto', 'dry': 'Dry', 'fan': 'Fan'};

  @override
  Widget build(BuildContext context) {
    final c = device.cap('climate')!;
    final mode = c['mode'] as String? ?? 'off';
    final target = (c['target'] as num?)?.toDouble() ?? 21;
    final current = (c['current'] as num?)?.toDouble();
    final fan = c['fan'] as String?;
    final modes = (c['modes'] as List?)?.cast<String>() ?? const ['off', 'heat', 'cool', 'auto'];
    final fans = (c['fans'] as List?)?.cast<String>() ?? const [];
    final min = (c['min'] as num?)?.toDouble() ?? 16;
    final max = (c['max'] as num?)?.toDouble() ?? 30;
    final step = (c['step'] as num?)?.toDouble() ?? 0.5;
    final sp = Spacing.of(context);
    final theme = Theme.of(context);

    // The hub wants mode and target in every command (device.rs's Climate);
    // what the device offers (modes, range) it keeps by itself.
    void send({String? m, double? t, String? f}) => runRequest(
        context,
        () => hub.command(device.id, 'climate', {
              'mode': m ?? mode,
              'target': t ?? target,
              if ((f ?? fan) != null) 'fan': f ?? fan,
            }));

    String fmt(double v) => v == v.roundToDouble() ? v.toStringAsFixed(0) : v.toStringAsFixed(1);

    return _Section(
      title: 'Climate',
      trailing: current == null ? null : Text('Room ${fmt(current)} °C'),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              IconButton.outlined(
                onPressed: target - step >= min - 1e-9 ? () => send(t: target - step) : null,
                icon: const Icon(Icons.remove),
              ),
              Padding(
                padding: EdgeInsets.symmetric(horizontal: sp.xl),
                child: Text('${fmt(target)} °C', style: theme.textTheme.displaySmall),
              ),
              IconButton.outlined(
                onPressed: target + step <= max + 1e-9 ? () => send(t: target + step) : null,
                icon: const Icon(Icons.add),
              ),
            ],
          ),
          SizedBox(height: sp.m),
          Wrap(
            spacing: sp.s,
            runSpacing: sp.s,
            children: [
              for (final m in modes)
                ChoiceChip(label: Text(_modeLabels[m] ?? m), selected: m == mode, onSelected: (_) => send(m: m)),
            ],
          ),
          if (fans.isNotEmpty) ...[
            SizedBox(height: sp.m),
            Row(children: [
              const Text('Fan'),
              SizedBox(width: sp.l),
              DropdownButton<String>(
                value: fans.contains(fan) ? fan : null,
                items: [for (final f in fans) DropdownMenuItem(value: f, child: Text(f))],
                onChanged: (f) => send(f: f),
              ),
            ]),
          ],
        ],
      ),
    );
  }
}

// ---- lock ----

class _LockControl extends StatelessWidget {
  const _LockControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  /// THE UNLOCK RULE (#77): opening a door never happens by a stray tap.
  /// The person confirms first; only then is "confirmed": true sent --
  /// the hub refuses an unlock without it.
  Future<void> _unlock(BuildContext context) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Unlock ${device.name}?'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Unlock')),
        ],
      ),
    );
    if (ok == true && context.mounted) {
      await runRequest(context, () => hub.command(device.id, 'lock', {'state': 'unlocked', 'confirmed': true}));
    }
  }

  @override
  Widget build(BuildContext context) {
    final state = device.cap('lock')!['state'] as String? ?? 'unknown';
    final sp = Spacing.of(context);
    final scheme = Theme.of(context).colorScheme;
    final (label, icon, color) = switch (state) {
      'locked' => ('Locked', Icons.lock, scheme.onSurface),
      'unlocked' => ('Unlocked', Icons.lock_open, scheme.primary),
      'jammed' => ('Jammed — check the door', Icons.warning_amber, scheme.error),
      _ => ('Unknown', Icons.help_outline, scheme.onSurfaceVariant),
    };
    return _Section(
      title: 'Lock',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(children: [Icon(icon, color: color), SizedBox(width: sp.s), Text(label, style: TextStyle(color: color))]),
          SizedBox(height: sp.m),
          Row(
            children: [
              Expanded(
                child: OutlinedButton.icon(
                  onPressed: () => runRequest(context, () => hub.command(device.id, 'lock', {'state': 'locked'})),
                  icon: const Icon(Icons.lock),
                  label: const Text('Lock'),
                ),
              ),
              SizedBox(width: sp.s),
              Expanded(
                child: FilledButton.icon(
                  onPressed: () => _unlock(context),
                  icon: const Icon(Icons.lock_open),
                  label: const Text('Unlock…'),
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

// ---- media ----

class _MediaControl extends StatelessWidget {
  const _MediaControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final m = device.cap('media')!;
    final volume = (m['volume'] as num?)?.toInt() ?? 0;
    final muted = m['muted'] as bool? ?? false;
    final input = m['input'] as String? ?? '';
    final inputs = ((m['inputs'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final app = m['app'] as Map<String, dynamic>?;
    final channel = m['channel'] as Map<String, dynamic>?;
    final sp = Spacing.of(context);

    // volume and muted are required in a media command (device.rs's Media);
    // inputs / app / channel only come from the device.
    void send({int? v, bool? mute, String? inp}) => runRequest(
        context, () => hub.command(device.id, 'media', {'volume': v ?? volume, 'muted': mute ?? muted, 'input': inp ?? input}));

    final nowShowing = [
      if (app != null) app['label'],
      if (channel != null) '${channel['number']} ${channel['name']}'.trim(),
    ].join(' · ');

    return _Section(
      title: 'Sound and source',
      trailing: nowShowing.isEmpty ? null : Flexible(child: Text(nowShowing, overflow: TextOverflow.ellipsis)),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(children: [
            IconButton(
              onPressed: () => send(mute: !muted),
              icon: Icon(muted ? Icons.volume_off : Icons.volume_up),
              tooltip: muted ? 'Unmute' : 'Mute',
            ),
            Expanded(
              child: _LevelSlider(
                value: volume.toDouble(),
                divisions: 100,
                label: (v) => '${v.round()}',
                onCommit: (v) => send(v: v.round()),
              ),
            ),
          ]),
          if (inputs.isNotEmpty) ...[
            SizedBox(height: sp.s),
            Wrap(
              spacing: sp.s,
              runSpacing: sp.s,
              children: [
                for (final i in inputs)
                  ChoiceChip(
                    label: Text(i['label'] as String? ?? i['id'] as String),
                    selected: i['id'] == input,
                    onSelected: (_) => send(inp: i['id'] as String),
                  ),
              ],
            ),
          ],
        ],
      ),
    );
  }
}

// ---- remote ----

class _RemoteControl extends StatefulWidget {
  const _RemoteControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_RemoteControl> createState() => _RemoteControlState();
}

class _RemoteControlState extends State<_RemoteControl> {
  final _text = TextEditingController();

  @override
  void dispose() {
    _text.dispose();
    super.dispose();
  }

  void _press(String button) =>
      runRequest(context, () => widget.hub.action(widget.device.id, 'remote', 'press', {'button': button}));

  Future<void> _type() async {
    final text = _text.text;
    if (text.isEmpty) return;
    // As on a phone keyboard: typing AND sending it (hub_ws.py does the same).
    await runRequest(context, () async {
      await widget.hub.action(widget.device.id, 'remote', 'type', {'text': text});
      await widget.hub.action(widget.device.id, 'remote', 'submit');
    });
    _text.clear();
  }

  @override
  Widget build(BuildContext context) {
    final r = widget.device.cap('remote')!;
    final buttons = (r['buttons'] as List?)?.cast<String>() ?? const [];
    final keyboard = r['keyboard'] as bool? ?? false;
    final brightness = r['brightness'] as Map<String, dynamic>?;
    final has = buttons.toSet();
    final sp = Spacing.of(context);

    Widget key(String name, {IconData? icon, String? label}) {
      final enabled = has.contains(name);
      return Padding(
        padding: EdgeInsets.all(sp.xs / 2),
        child: icon != null
            ? IconButton.filledTonal(onPressed: enabled ? () => _press(name) : null, icon: Icon(icon), tooltip: name)
            : FilledButton.tonal(onPressed: enabled ? () => _press(name) : null, child: Text(label ?? name)),
      );
    }

    final hasDpad = const ['UP', 'DOWN', 'LEFT', 'RIGHT', 'OK'].any(has.contains);
    final hasTransport = const ['VOLUME_UP', 'VOLUME_DOWN', 'MUTE', 'CHANNEL_UP', 'CHANNEL_DOWN'].any(has.contains);
    final hasPlayback = const ['PLAY', 'PAUSE', 'STOP', 'REWIND', 'FAST_FORWARD'].any(has.contains);
    final digits = [for (var i = 0; i <= 9; i++) '$i'].where(has.contains).toList();
    final colours = const ['RED', 'GREEN', 'YELLOW', 'BLUE'].where(has.contains).toList();
    final menuKeys = const ['BACK', 'HOME', 'MENU', 'EXIT', 'INFO'].where(has.contains).toList();
    // A learning remote's own buttons ("Power", "Red"...): a labelled grid.
    final named = buttons.where((b) => !remoteLayoutButtons.contains(b)).toList();

    return _Section(
      title: 'Remote',
      child: Column(
        children: [
          if (brightness != null)
            Row(mainAxisAlignment: MainAxisAlignment.center, children: [
              IconButton.filledTonal(onPressed: () => _press(brightness['down'] as String), icon: const Icon(Icons.brightness_low), tooltip: 'Dimmer'),
              SizedBox(width: sp.xl),
              IconButton.filledTonal(onPressed: () => _press(brightness['up'] as String), icon: const Icon(Icons.brightness_high), tooltip: 'Brighter'),
            ]),
          if (hasDpad)
            Column(children: [
              key('UP', icon: Icons.keyboard_arrow_up),
              Row(mainAxisAlignment: MainAxisAlignment.center, children: [
                key('LEFT', icon: Icons.keyboard_arrow_left),
                key('OK', label: 'OK'),
                key('RIGHT', icon: Icons.keyboard_arrow_right),
              ]),
              key('DOWN', icon: Icons.keyboard_arrow_down),
            ]),
          if (menuKeys.isNotEmpty)
            Wrap(alignment: WrapAlignment.center, children: [
              for (final k in menuKeys)
                key(k, icon: switch (k) { 'BACK' => Icons.arrow_back, 'HOME' => Icons.home, 'MENU' => Icons.menu, 'INFO' => Icons.info_outline, _ => Icons.close }),
            ]),
          if (hasTransport)
            Row(mainAxisAlignment: MainAxisAlignment.center, children: [
              Column(children: [key('VOLUME_UP', icon: Icons.volume_up), key('VOLUME_DOWN', icon: Icons.volume_down)]),
              key('MUTE', icon: Icons.volume_off),
              Column(children: [key('CHANNEL_UP', icon: Icons.keyboard_double_arrow_up), key('CHANNEL_DOWN', icon: Icons.keyboard_double_arrow_down)]),
            ]),
          if (hasPlayback)
            Wrap(alignment: WrapAlignment.center, children: [
              key('REWIND', icon: Icons.fast_rewind),
              key('PLAY', icon: Icons.play_arrow),
              key('PAUSE', icon: Icons.pause),
              key('STOP', icon: Icons.stop),
              key('FAST_FORWARD', icon: Icons.fast_forward),
            ]),
          if (digits.isNotEmpty) Wrap(alignment: WrapAlignment.center, children: [for (final d in digits) key(d)]),
          if (colours.isNotEmpty)
            Wrap(alignment: WrapAlignment.center, children: [
              for (final c in colours)
                Padding(
                  padding: EdgeInsets.all(sp.xs / 2),
                  child: FilledButton(
                    style: FilledButton.styleFrom(backgroundColor: _parseHex(switch (c) {
                      'RED' => '#DC2626',
                      'GREEN' => '#16A34A',
                      'YELLOW' => '#CA8A04',
                      _ => '#2563EB',
                    }), foregroundColor: Colors.white),
                    onPressed: () => _press(c),
                    child: const SizedBox(width: 8),
                  ),
                ),
            ]),
          if (named.isNotEmpty)
            Wrap(alignment: WrapAlignment.center, children: [for (final b in named) key(b)]),
          if (keyboard) ...[
            SizedBox(height: sp.m),
            Row(children: [
              Expanded(
                child: TextField(
                  controller: _text,
                  decoration: const InputDecoration(labelText: 'Type on the TV'),
                  onSubmitted: (_) => _type(),
                ),
              ),
              IconButton(onPressed: _type, icon: const Icon(Icons.send)),
            ]),
          ],
        ],
      ),
    );
  }
}

// ---- sensor / energy: only read ----

class _SensorReadings extends StatelessWidget {
  const _SensorReadings({required this.device});
  final Device device;

  @override
  Widget build(BuildContext context) {
    final readings = (device.cap('sensor')!['readings'] as Map?)?.cast<String, dynamic>() ?? const {};
    return _Section(
      title: 'Readings',
      child: Column(children: [
        for (final r in readings.entries)
          _ValueRow(label: _humanize(r.key), value: '${_num((r.value as Map)['value'])} ${(r.value as Map)['unit'] ?? ''}'.trim()),
      ]),
    );
  }
}

class _EnergyReadings extends StatelessWidget {
  const _EnergyReadings({required this.device});
  final Device device;

  @override
  Widget build(BuildContext context) {
    final e = device.cap('energy')!;
    return _Section(
      title: 'Energy',
      child: Column(children: [
        _ValueRow(label: 'Power', value: '${_num(e['power_w'])} W'),
        if (e['energy_kwh'] != null) _ValueRow(label: 'Energy', value: '${_num(e['energy_kwh'], 3)} kWh'),
        if (e['voltage_v'] != null) _ValueRow(label: 'Voltage', value: '${_num(e['voltage_v'])} V'),
        if (e['current_a'] != null) _ValueRow(label: 'Current', value: '${_num(e['current_a'], 2)} A'),
      ]),
    );
  }
}

/// A capability this app version doesn't know (a newer hub's "vacuum",
/// #45): shown as it is, read-only, rather than hidden.
class _UnknownCapability extends StatelessWidget {
  const _UnknownCapability({required this.name, required this.state});
  final String name;
  final Map<String, dynamic> state;

  @override
  Widget build(BuildContext context) {
    return _Section(
      title: _humanize(name),
      trailing: Text('Update the app to control this', style: Theme.of(context).textTheme.bodySmall),
      child: Column(children: [
        for (final e in state.entries)
          _ValueRow(label: _humanize(e.key), value: e.value is num ? _num(e.value) : (e.value is String ? e.value as String : jsonEncode(e.value))),
      ]),
    );
  }
}

class _ValueRow extends StatelessWidget {
  const _ValueRow({required this.label, required this.value});
  final String label, value;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.symmetric(vertical: Spacing.of(context).xs / 2),
      child: Row(children: [
        Expanded(child: Text(label, style: Theme.of(context).textTheme.bodySmall)),
        Flexible(child: Text(value, textAlign: TextAlign.end)),
      ]),
    );
  }
}

String _num(Object? v, [int decimals = 1]) {
  if (v is! num) return '—';
  return v == v.roundToDouble() ? v.toStringAsFixed(0) : v.toStringAsFixed(decimals);
}

String _humanize(String key) {
  final s = key.replaceAll('_', ' ');
  return s.isEmpty ? s : s[0].toUpperCase() + s.substring(1);
}
