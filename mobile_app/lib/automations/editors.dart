import 'dart:convert';

import 'package:flutter/material.dart';

import '../hub/device.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import 'model.dart';

/// The pieces of the scene and automation editors: one dialog per kind of
/// item. Each returns the JSON the hub expects, or null when cancelled.

// ---- shared bits ----

class DaysPicker extends StatelessWidget {
  const DaysPicker({super.key, required this.days, required this.onChanged});
  final List<String> days;
  final ValueChanged<List<String>> onChanged;

  @override
  Widget build(BuildContext context) {
    return Wrap(spacing: 4, runSpacing: 4, children: [
      for (final w in weekdays)
        FilterChip(
          label: Text(weekdayLabels[w]!),
          selected: days.contains(w),
          onSelected: (on) => onChanged([for (final x in weekdays) if (x == w ? on : days.contains(x)) x]),
        ),
    ]);
  }
}

class TimeField extends StatelessWidget {
  const TimeField({super.key, required this.label, required this.value, required this.onChanged, this.clearable = false});
  final String label;
  final String? value;
  final ValueChanged<String?> onChanged;
  final bool clearable;

  @override
  Widget build(BuildContext context) {
    return Row(children: [
      Expanded(child: Text(label)),
      OutlinedButton(
        onPressed: () async {
          final parts = (value ?? '07:00').split(':');
          final t = await showTimePicker(
            context: context,
            initialTime: TimeOfDay(hour: int.parse(parts[0]), minute: int.parse(parts[1])),
            builder: (context, child) => MediaQuery(data: MediaQuery.of(context).copyWith(alwaysUse24HourFormat: true), child: child!),
          );
          if (t != null) onChanged(hhmm(t.hour, t.minute));
        },
        child: Text(value ?? 'Any'),
      ),
      if (clearable && value != null) IconButton(onPressed: () => onChanged(null), icon: const Icon(Icons.clear)),
    ]);
  }
}

class DevicePicker extends StatelessWidget {
  const DevicePicker({super.key, required this.hub, required this.value, required this.onChanged, this.where});
  final HubStore hub;
  final String? value;
  final ValueChanged<String?> onChanged;
  final bool Function(Device)? where;

  @override
  Widget build(BuildContext context) {
    final devices = hub.devices.where(where ?? (_) => true).toList()..sort((a, b) => a.name.compareTo(b.name));
    return DropdownButtonFormField<String>(
      initialValue: devices.any((d) => d.id == value) ? value : null,
      isExpanded: true,
      decoration: const InputDecoration(labelText: 'Device'),
      items: [
        for (final d in devices)
          DropdownMenuItem(value: d.id, child: Text(d.room.isEmpty ? d.name : '${d.name} · ${d.room}', overflow: TextOverflow.ellipsis)),
      ],
      onChanged: onChanged,
    );
  }
}

Future<T?> _sheet<T>(BuildContext context, String title, Widget Function(BuildContext, StateSetter) body,
    {required T? Function() result}) {
  return showDialog<T>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setState) => AlertDialog(
        title: Text(title),
        scrollable: true,
        content: SizedBox(width: 420, child: body(context, setState)),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context), child: const Text('Cancel')),
          FilledButton(
            onPressed: () {
              final r = result();
              if (r != null) Navigator.pop(context, r);
            },
            child: const Text('OK'),
          ),
        ],
      ),
    ),
  );
}

/// The device + capability + field of a state/threshold trigger or a
/// state/value condition, picked from what the device really reports.
class _FieldPicker extends StatelessWidget {
  const _FieldPicker({required this.hub, required this.item, required this.onChanged, this.numbersOnly = false});
  final HubStore hub;
  final Map<String, dynamic> item;
  final VoidCallback onChanged;
  final bool numbersOnly;

  @override
  Widget build(BuildContext context) {
    final d = hub.device('${item['device']}');
    final fields = d == null ? <String, Map<String, Object?>>{} : watchableFields(d);
    final options = <String, Object?>{
      for (final c in fields.entries)
        for (final f in c.value.entries)
          if (!numbersOnly || f.value is num) '${c.key}|${f.key}': f.value,
    };
    final current = '${item['capability']}|${item['field']}';
    return Column(children: [
      DevicePicker(
        hub: hub,
        value: item['device'] as String?,
        onChanged: (id) {
          item
            ..['device'] = id
            ..remove('capability')
            ..remove('field');
          onChanged();
        },
      ),
      if (d != null)
        DropdownButtonFormField<String>(
          initialValue: options.containsKey(current) ? current : null,
          isExpanded: true,
          decoration: const InputDecoration(labelText: 'What'),
          items: [
            for (final o in options.entries)
              DropdownMenuItem(value: o.key, child: Text('${o.key.replaceFirst('|', ' · ')}   (now ${showValue(o.value)})', overflow: TextOverflow.ellipsis)),
          ],
          onChanged: (v) {
            final parts = v!.split('|');
            item
              ..['capability'] = parts[0]
              ..['field'] = parts[1];
            onChanged();
          },
        ),
      if (d != null && options.isEmpty)
        Padding(
          padding: const EdgeInsets.only(top: 8),
          child: Text(numbersOnly ? 'This device reports no numbers.' : 'This device reports nothing to watch.'),
        ),
    ]);
  }
}

/// The value a state must have: true/false as a switch-like choice, a
/// number field, or text -- by what the field holds now.
class _ValueInput extends StatelessWidget {
  const _ValueInput({required this.label, required this.example, required this.value, required this.onChanged, this.optional = false});
  final String label;
  final Object? example;
  final Object? value;
  final ValueChanged<Object?> onChanged;
  final bool optional;

  @override
  Widget build(BuildContext context) {
    if (example is bool || value is bool) {
      return DropdownButtonFormField<Object?>(
        initialValue: value is bool ? value : null,
        decoration: InputDecoration(labelText: label),
        items: [
          if (optional) const DropdownMenuItem(value: null, child: Text('Any')),
          const DropdownMenuItem(value: true, child: Text('true (on)')),
          const DropdownMenuItem(value: false, child: Text('false (off)')),
        ],
        onChanged: onChanged,
      );
    }
    return TextFormField(
      initialValue: value == null ? '' : showValue(value),
      keyboardType: example is num ? const TextInputType.numberWithOptions(decimal: true, signed: true) : TextInputType.text,
      decoration: InputDecoration(labelText: label, helperText: optional ? 'Empty = any' : null),
      onChanged: (t) => onChanged(t.trim().isEmpty ? null : parseLoose(t)),
    );
  }
}

Object? _exampleOf(HubStore hub, Map<String, dynamic> item) {
  final d = hub.device('${item['device']}');
  if (d == null) return null;
  return watchableFields(d)[item['capability']]?[item['field']];
}

String? _missingField(Map<String, dynamic> item) =>
    item['device'] == null ? 'Pick a device.' : (item['field'] == null ? 'Pick what to watch.' : null);

void _snack(BuildContext context, String text) => ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(text)));

// ---- triggers ----

Future<Map<String, dynamic>?> editTrigger(BuildContext context, HubStore hub, [Map<String, dynamic>? initial]) {
  final t = Map<String, dynamic>.from(initial ?? {'type': 'time', 'at': '07:00'});
  final hasLocation = hub.settings['latitude'] != null;
  return _sheet<Map<String, dynamic>>(
    context,
    'When',
    (context, setState) {
      final sp = Spacing.of(context);
      void retype(String type) => setState(() {
            t
              ..clear()
              ..addAll(switch (type) {
                'time' => {'type': 'time', 'at': '07:00'},
                'sun' => {'type': 'sun', 'event': 'sunset', 'offset_min': 0},
                'state' => {'type': 'state'},
                _ => {'type': 'threshold'},
              });
          });
      return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        SegmentedButton<String>(
          segments: const [
            ButtonSegment(value: 'time', label: Text('Time')),
            ButtonSegment(value: 'sun', label: Text('Sun')),
            ButtonSegment(value: 'state', label: Text('Device')),
            ButtonSegment(value: 'threshold', label: Text('Limit')),
          ],
          selected: {t['type'] as String},
          showSelectedIcon: false,
          onSelectionChanged: (s) => retype(s.first),
        ),
        SizedBox(height: sp.l),
        ...switch (t['type']) {
          'time' => [
              TimeField(label: 'At', value: t['at'] as String?, onChanged: (v) => setState(() => t['at'] = v)),
              SizedBox(height: sp.m),
              const Text('On (none = every day)'),
              DaysPicker(days: ((t['days'] as List?) ?? const []).cast<String>(), onChanged: (d) => setState(() => d.isEmpty ? t.remove('days') : t['days'] = d)),
            ],
          'sun' => [
              if (!hasLocation)
                Text('Set the hub\'s location first (Hub settings): sunrise and sunset depend on it.',
                    style: TextStyle(color: Theme.of(context).colorScheme.error)),
              SegmentedButton<String>(
                segments: const [ButtonSegment(value: 'sunrise', label: Text('Sunrise')), ButtonSegment(value: 'sunset', label: Text('Sunset'))],
                selected: {t['event'] as String},
                onSelectionChanged: (s) => setState(() => t['event'] = s.first),
              ),
              TextFormField(
                initialValue: '${t['offset_min'] ?? 0}',
                keyboardType: const TextInputType.numberWithOptions(signed: true),
                decoration: const InputDecoration(labelText: 'Minutes before (−) or after (+)'),
                onChanged: (v) => t['offset_min'] = int.tryParse(v.trim()) ?? 0,
              ),
              SizedBox(height: sp.m),
              const Text('On (none = every day)'),
              DaysPicker(days: ((t['days'] as List?) ?? const []).cast<String>(), onChanged: (d) => setState(() => d.isEmpty ? t.remove('days') : t['days'] = d)),
            ],
          'state' => [
              _FieldPicker(hub: hub, item: t, onChanged: () => setState(() {})),
              if (t['field'] != null) ...[
                _ValueInput(
                  label: 'Becomes',
                  optional: true,
                  example: _exampleOf(hub, t),
                  value: t['to'],
                  onChanged: (v) => setState(() => v == null ? t.remove('to') : t['to'] = v),
                ),
                _ValueInput(
                  label: 'From',
                  optional: true,
                  example: _exampleOf(hub, t),
                  value: t['from'],
                  onChanged: (v) => setState(() => v == null ? t.remove('from') : t['from'] = v),
                ),
              ],
            ],
          _ => [
              _FieldPicker(hub: hub, item: t, numbersOnly: true, onChanged: () => setState(() {})),
              if (t['field'] != null) ...[
                SegmentedButton<String>(
                  segments: const [ButtonSegment(value: 'above', label: Text('Goes above')), ButtonSegment(value: 'below', label: Text('Goes below'))],
                  selected: {t.containsKey('below') ? 'below' : 'above'},
                  onSelectionChanged: (s) => setState(() {
                    final v = t.remove('above') ?? t.remove('below') ?? 0;
                    t[s.first] = v;
                  }),
                ),
                TextFormField(
                  initialValue: '${t['above'] ?? t['below'] ?? ''}',
                  keyboardType: const TextInputType.numberWithOptions(decimal: true, signed: true),
                  decoration: const InputDecoration(labelText: 'Limit', helperText: 'Once per crossing, not on every report'),
                  onChanged: (v) => t[t.containsKey('below') ? 'below' : 'above'] = num.tryParse(v.trim()),
                ),
              ],
            ],
        },
      ]);
    },
    result: () {
      if (t['type'] == 'state' || t['type'] == 'threshold') {
        final missing = _missingField(t);
        if (missing != null) {
          _snack(context, missing);
          return null;
        }
      }
      if (t['type'] == 'threshold' && t['above'] == null && t['below'] == null) {
        _snack(context, 'Enter the limit.');
        return null;
      }
      return t;
    },
  );
}

// ---- conditions ----

Future<Map<String, dynamic>?> editCondition(BuildContext context, HubStore hub, [Map<String, dynamic>? initial]) {
  final c = Map<String, dynamic>.from(initial ?? {'type': 'time', 'after': '18:00', 'before': '23:00'});
  return _sheet<Map<String, dynamic>>(
    context,
    'Only if',
    (context, setState) {
      final sp = Spacing.of(context);
      return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        SegmentedButton<String>(
          segments: const [
            ButtonSegment(value: 'time', label: Text('Time')),
            ButtonSegment(value: 'sun', label: Text('Day/night')),
            ButtonSegment(value: 'state', label: Text('Device')),
            ButtonSegment(value: 'value', label: Text('Number')),
          ],
          selected: {c['type'] as String},
          showSelectedIcon: false,
          onSelectionChanged: (s) => setState(() {
            c
              ..clear()
              ..addAll(switch (s.first) {
                'time' => {'type': 'time', 'after': '18:00', 'before': '23:00'},
                'sun' => {'type': 'sun', 'is': 'night'},
                'state' => {'type': 'state'},
                _ => {'type': 'value'},
              });
          }),
        ),
        SizedBox(height: sp.l),
        ...switch (c['type']) {
          'time' => [
              TimeField(label: 'After', value: c['after'] as String?, clearable: true, onChanged: (v) => setState(() => v == null ? c.remove('after') : c['after'] = v)),
              TimeField(label: 'Before', value: c['before'] as String?, clearable: true, onChanged: (v) => setState(() => v == null ? c.remove('before') : c['before'] = v)),
              Text('Across midnight works too (after 22:00, before 06:00).', style: Theme.of(context).textTheme.bodySmall),
              SizedBox(height: sp.m),
              const Text('On (none = every day)'),
              DaysPicker(days: ((c['days'] as List?) ?? const []).cast<String>(), onChanged: (d) => setState(() => d.isEmpty ? c.remove('days') : c['days'] = d)),
            ],
          'sun' => [
              SegmentedButton<String>(
                segments: const [ButtonSegment(value: 'day', label: Text('During the day')), ButtonSegment(value: 'night', label: Text('At night'))],
                selected: {c['is'] as String},
                onSelectionChanged: (s) => setState(() => c['is'] = s.first),
              ),
            ],
          'state' => [
              _FieldPicker(hub: hub, item: c, onChanged: () => setState(() {})),
              if (c['field'] != null)
                _ValueInput(label: 'Is', example: _exampleOf(hub, c), value: c['is'], onChanged: (v) => setState(() => c['is'] = v)),
            ],
          _ => [
              _FieldPicker(hub: hub, item: c, numbersOnly: true, onChanged: () => setState(() {})),
              if (c['field'] != null) ...[
                TextFormField(
                  initialValue: '${c['above'] ?? ''}',
                  keyboardType: const TextInputType.numberWithOptions(decimal: true, signed: true),
                  decoration: const InputDecoration(labelText: 'Above (optional)'),
                  onChanged: (v) => num.tryParse(v.trim()) == null ? c.remove('above') : c['above'] = num.parse(v.trim()),
                ),
                TextFormField(
                  initialValue: '${c['below'] ?? ''}',
                  keyboardType: const TextInputType.numberWithOptions(decimal: true, signed: true),
                  decoration: const InputDecoration(labelText: 'Below (optional)'),
                  onChanged: (v) => num.tryParse(v.trim()) == null ? c.remove('below') : c['below'] = num.parse(v.trim()),
                ),
              ],
            ],
        },
      ]);
    },
    result: () {
      if (c['type'] == 'state' || c['type'] == 'value') {
        final missing = _missingField(c);
        if (missing != null) {
          _snack(context, missing);
          return null;
        }
      }
      if (c['type'] == 'state' && !c.containsKey('is')) {
        _snack(context, 'Enter the value it must have.');
        return null;
      }
      if (c['type'] == 'value' && c['above'] == null && c['below'] == null) {
        _snack(context, 'Enter above, below, or both.');
        return null;
      }
      return c;
    },
  );
}

// ---- steps ----

/// A step: set a device's capability, run one of its actions, or (in an
/// automation) run a scene.
Future<Map<String, dynamic>?> editStep(BuildContext context, HubStore hub,
    {Map<String, dynamic>? initial, required bool allowScene}) {
  final s = Map<String, dynamic>.from(initial ?? {});
  var kind = s['scene'] != null ? 'scene' : (s['action'] != null ? 'action' : 'set');
  var advanced = false;
  var advancedText = '';
  String? advancedError;

  return _sheet<Map<String, dynamic>>(
    context,
    'Do',
    (context, setState) {
      final sp = Spacing.of(context);
      final d = hub.device('${s['device']}');
      final settable = d == null ? <String>[] : settableCapabilities.where(d.has).toList();
      final actionable = d == null ? <String>[] : capabilityActions.keys.where(d.has).toList();
      final caps = kind == 'action' ? actionable : settable;
      return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        SegmentedButton<String>(
          segments: [
            const ButtonSegment(value: 'set', label: Text('Set')),
            const ButtonSegment(value: 'action', label: Text('Action')),
            if (allowScene) const ButtonSegment(value: 'scene', label: Text('Scene')),
          ],
          selected: {kind},
          showSelectedIcon: false,
          onSelectionChanged: (x) => setState(() {
            kind = x.first;
            s
              ..remove('scene')
              ..remove('value')
              ..remove('action')
              ..remove('args')
              ..remove('capability');
            if (kind == 'scene') s.remove('device');
          }),
        ),
        SizedBox(height: sp.l),
        if (kind == 'scene')
          DropdownButtonFormField<String>(
            initialValue: hub.scenes.any((x) => x['id'] == s['scene']) ? s['scene'] as String : null,
            decoration: const InputDecoration(labelText: 'Scene'),
            items: [for (final sc in hub.scenes) DropdownMenuItem(value: sc['id'] as String, child: Text(sc['name'] as String))],
            onChanged: (v) => setState(() => s['scene'] = v),
          )
        else ...[
          DevicePicker(
            hub: hub,
            value: s['device'] as String?,
            where: (x) => (kind == 'action' ? capabilityActions.keys : settableCapabilities).any(x.has),
            onChanged: (id) => setState(() {
              s
                ..['device'] = id
                ..remove('capability')
                ..remove('value')
                ..remove('action')
                ..remove('args');
            }),
          ),
          if (d != null)
            DropdownButtonFormField<String>(
              initialValue: caps.contains(s['capability']) ? s['capability'] as String : null,
              decoration: const InputDecoration(labelText: 'Capability'),
              items: [for (final c in caps) DropdownMenuItem(value: c, child: Text(c))],
              onChanged: (c) => setState(() {
                s
                  ..['capability'] = c
                  ..remove('value')
                  ..remove('action')
                  ..remove('args');
                if (kind == 'set') s['value'] = defaultValue(d, c!);
                if (kind == 'action') s['action'] = capabilityActions[c]!.first;
              }),
            ),
          if (d != null && s['capability'] != null) ...[
            SizedBox(height: sp.m),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Edit as JSON'),
              value: advanced,
              onChanged: (v) => setState(() {
                advanced = v;
                advancedText = const JsonEncoder.withIndent('  ').convert(kind == 'set' ? s['value'] : (s['args'] ?? {}));
                advancedError = null;
              }),
            ),
            if (advanced)
              TextFormField(
                initialValue: advancedText,
                maxLines: 6,
                style: const TextStyle(fontFamily: 'monospace'),
                decoration: InputDecoration(labelText: kind == 'set' ? 'value' : 'args', errorText: advancedError),
                onChanged: (t) {
                  try {
                    final v = jsonDecode(t);
                    if (kind == 'set') {
                      s['value'] = v;
                    } else {
                      s['args'] = v;
                    }
                    setState(() => advancedError = null);
                  } catch (_) {
                    setState(() => advancedError = 'not valid JSON');
                  }
                },
              )
            else if (kind == 'set')
              CommandValueEditor(device: d, capability: s['capability'] as String, value: s['value'], onChanged: (v) => setState(() => s['value'] = v))
            else
              _ActionEditor(device: d, step: s, onChanged: () => setState(() {})),
          ],
        ],
      ]);
    },
    result: () {
      if (kind == 'scene') {
        if (s['scene'] == null) {
          _snack(context, 'Pick a scene.');
          return null;
        }
        return {'scene': s['scene']};
      }
      if (s['device'] == null || s['capability'] == null) {
        _snack(context, 'Pick a device and what to do.');
        return null;
      }
      if (advancedError != null) {
        _snack(context, 'Fix the JSON first.');
        return null;
      }
      final args = s['args'];
      return kind == 'set'
          ? {'device': s['device'], 'capability': s['capability'], 'value': s['value']}
          : {'device': s['device'], 'capability': s['capability'], 'action': s['action'], if (args is Map && args.isNotEmpty) 'args': args};
    },
  );
}

/// A command's starting value: the device's state now, minus what only the
/// device reports (device.rs ignores or refuses those in a command).
Object? defaultValue(Device d, String capability) {
  final now = Map<String, dynamic>.from(d.cap(capability) ?? {});
  return switch (capability) {
    'switch' => {'on': true},
    'dimmer' => {'level': now['level'] ?? 50},
    'color' => now['hex'] != null ? {'hex': now['hex']} : {'kelvin': now['kelvin'] ?? 2700},
    'cover' => {'position': now['position'] ?? 0},
    'climate' => {'mode': now['mode'] ?? 'heat', 'target': now['target'] ?? 21, if (now['fan'] != null) 'fan': now['fan']},
    'lock' => {'state': 'locked'},
    'media' => {'volume': now['volume'] ?? 10, 'muted': now['muted'] ?? false, 'input': now['input'] ?? ''},
    _ => now,
  };
}

/// The value of a "set" step, as the same controls the device page has.
class CommandValueEditor extends StatelessWidget {
  const CommandValueEditor({super.key, required this.device, required this.capability, required this.value, required this.onChanged});
  final Device device;
  final String capability;
  final Object? value;
  final ValueChanged<Object?> onChanged;

  @override
  Widget build(BuildContext context) {
    final v = Map<String, dynamic>.from((value as Map?) ?? {});
    final cap = device.cap(capability) ?? const {};
    void set(String k, Object? x) => onChanged({...v, k: x});
    switch (capability) {
      case 'switch':
        return SwitchListTile(contentPadding: EdgeInsets.zero, title: Text(v['on'] == true ? 'On' : 'Off'), value: v['on'] == true, onChanged: (x) => set('on', x));
      case 'dimmer':
        final level = ((v['level'] as num?) ?? 50).toDouble();
        return Row(children: [
          Expanded(child: Slider(value: level, max: 100, divisions: 100, label: '${level.round()} %', onChanged: (x) => set('level', x.round()))),
          Text('${level.round()} %'),
        ]);
      case 'cover':
        final pos = ((v['position'] as num?) ?? 0).toDouble();
        if (cap['can_position'] != true) {
          return const Text('This cover only opens, closes and stops: use an Action step.');
        }
        return Row(children: [
          Expanded(child: Slider(value: pos, max: 100, divisions: 100, label: '${pos.round()} % open', onChanged: (x) => set('position', x.round()))),
          Text('${pos.round()} %'),
        ]);
      case 'color':
        final palette = ((cap['palette'] as List?) ?? const ['#FF0000', '#FF8800', '#FFDD00', '#00FF00', '#00FFFF', '#0000FF', '#8800FF', '#FF00AA']).cast<String>();
        return Wrap(spacing: 6, runSpacing: 6, children: [
          for (final h in palette)
            ChoiceChip(
              label: Text(h),
              avatar: CircleAvatar(backgroundColor: Color(int.parse('FF${h.substring(1)}', radix: 16))),
              selected: v['hex'] == h,
              onSelected: (_) => onChanged({'hex': h}),
            ),
          if ((cap['palette'] as List?)?.isEmpty ?? true)
            for (final k in const [2700, 4000, 6500])
              ChoiceChip(label: Text('$k K'), selected: v['kelvin'] == k, onSelected: (_) => onChanged({'kelvin': k})),
        ]);
      case 'climate':
        final modes = ((cap['modes'] as List?) ?? const ['off', 'heat', 'cool', 'auto']).cast<String>();
        final fans = ((cap['fans'] as List?) ?? const []).cast<String>();
        final step = (cap['step'] as num?)?.toDouble() ?? 0.5;
        final target = ((v['target'] as num?) ?? 21).toDouble();
        return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Wrap(spacing: 6, children: [
            for (final m in modes) ChoiceChip(label: Text(m), selected: v['mode'] == m, onSelected: (_) => set('mode', m)),
          ]),
          Row(children: [
            IconButton(onPressed: () => set('target', target - step), icon: const Icon(Icons.remove)),
            Text('${target.toStringAsFixed(step < 1 ? 1 : 0)} °C'),
            IconButton(onPressed: () => set('target', target + step), icon: const Icon(Icons.add)),
          ]),
          if (fans.isNotEmpty)
            DropdownButtonFormField<String>(
              initialValue: fans.contains(v['fan']) ? v['fan'] as String : null,
              decoration: const InputDecoration(labelText: 'Fan'),
              items: [for (final f in fans) DropdownMenuItem(value: f, child: Text(f))],
              onChanged: (f) => set('fan', f),
            ),
        ]);
      case 'lock':
        final unlock = v['state'] == 'unlocked';
        return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          SegmentedButton<String>(
            segments: const [ButtonSegment(value: 'locked', label: Text('Lock')), ButtonSegment(value: 'unlocked', label: Text('Unlock'))],
            selected: {unlock ? 'unlocked' : 'locked'},
            onSelectionChanged: (s) => onChanged(s.first == 'unlocked' ? {'state': 'unlocked', 'confirmed': true} : {'state': 'locked'}),
          ),
          if (unlock)
            Padding(
              padding: const EdgeInsets.only(top: 8),
              child: Text('This opens the door with nobody confirming it at that moment. Only for a lock where you '
                  'really want that.', style: TextStyle(color: Theme.of(context).colorScheme.error)),
            ),
        ]);
      case 'media':
        final inputs = ((cap['inputs'] as List?) ?? const []).cast<Map<String, dynamic>>();
        final volume = ((v['volume'] as num?) ?? 10).toDouble();
        return Column(children: [
          Row(children: [
            const Text('Volume'),
            Expanded(child: Slider(value: volume.clamp(0, 100), max: 100, divisions: 100, label: '${volume.round()}', onChanged: (x) => set('volume', x.round()))),
          ]),
          SwitchListTile(contentPadding: EdgeInsets.zero, title: const Text('Muted'), value: v['muted'] == true, onChanged: (x) => set('muted', x)),
          if (inputs.isNotEmpty)
            DropdownButtonFormField<String>(
              initialValue: inputs.any((i) => i['id'] == v['input']) ? v['input'] as String : null,
              decoration: const InputDecoration(labelText: 'Input'),
              items: [for (final i in inputs) DropdownMenuItem(value: i['id'] as String, child: Text(i['label'] as String? ?? i['id'] as String))],
              onChanged: (x) => set('input', x),
            ),
        ]);
    }
    return Text(showValue(value));
  }
}

class _ActionEditor extends StatelessWidget {
  const _ActionEditor({required this.device, required this.step, required this.onChanged});
  final Device device;
  final Map<String, dynamic> step;
  final VoidCallback onChanged;

  @override
  Widget build(BuildContext context) {
    final cap = step['capability'] as String;
    final actions = capabilityActions[cap] ?? const <String>[];
    final args = Map<String, dynamic>.from((step['args'] as Map?) ?? {});
    void setArgs(Map<String, dynamic> a) {
      step['args'] = a;
      onChanged();
    }

    final buttons = ((device.cap('remote')?['buttons'] as List?) ?? const []).cast<String>();
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      DropdownButtonFormField<String>(
        initialValue: actions.contains(step['action']) ? step['action'] as String : null,
        decoration: const InputDecoration(labelText: 'Action'),
        items: [for (final a in actions) DropdownMenuItem(value: a, child: Text(a))],
        onChanged: (a) {
          step
            ..['action'] = a
            ..remove('args');
          onChanged();
        },
      ),
      ...switch ('$cap.${step['action']}') {
        'remote.press' => [
            DropdownButtonFormField<String>(
              initialValue: buttons.contains(args['button']) ? args['button'] as String : null,
              decoration: const InputDecoration(labelText: 'Button'),
              items: [for (final b in buttons) DropdownMenuItem(value: b, child: Text(b))],
              onChanged: (b) => setArgs({'button': b}),
            ),
          ],
        'remote.type' => [
            TextFormField(initialValue: args['text'] as String? ?? '', decoration: const InputDecoration(labelText: 'Text'), onChanged: (t) => setArgs({'text': t})),
          ],
        'remote.light' => [
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Use as a light'),
              value: args['enabled'] != false,
              onChanged: (x) => setArgs({...args, 'enabled': x}),
            ),
          ],
        'media.launch' => [
            TextFormField(initialValue: args['app'] as String? ?? '', decoration: const InputDecoration(labelText: 'App id', helperText: 'e.g. netflix'), onChanged: (t) => setArgs({'app': t.trim()})),
          ],
        'media.tune' => [
            TextFormField(initialValue: args['channel'] as String? ?? '', decoration: const InputDecoration(labelText: 'Channel id'), onChanged: (t) => setArgs({'channel': t.trim()})),
          ],
        _ => const <Widget>[],
      },
    ]);
  }
}
