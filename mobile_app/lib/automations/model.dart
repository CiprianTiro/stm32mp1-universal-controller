import 'dart:convert';

import '../hub/device.dart';
import '../hub/hub_store.dart';

/// Scenes and automations as the hub keeps them (backend_daemon's
/// automations.rs, wiki "Automations"). Plain JSON maps throughout: the
/// editor builds exactly the shapes the hub checks when saving.
///
///   triggers    time {at, days?} | sun {event, offset_min, days?} |
///               state {device, capability, field, to?, from?} |
///               threshold {device, capability, field, above | below}
///   conditions  time {after?, before?, days?} | sun {is} |
///               state {device, capability, field, is} |
///               value {device, capability, field, above?, below?}
///   steps       {device, capability, value} | {device, capability, action, args?} |
///               {scene} (automations only)
const weekdays = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
const weekdayLabels = {'mon': 'Mon', 'tue': 'Tue', 'wed': 'Wed', 'thu': 'Thu', 'fri': 'Fri', 'sat': 'Sat', 'sun': 'Sun'};

/// Capabilities a step can SET (sensor and energy are only read).
const settableCapabilities = ['switch', 'dimmer', 'color', 'cover', 'climate', 'lock', 'media'];

/// Actions per capability (device.rs's check_action) a step can run.
const capabilityActions = {
  'cover': ['open', 'close', 'stop'],
  'remote': ['press', 'type', 'light'],
  'media': ['launch', 'tune'],
};

String deviceName(HubStore hub, Object? id) => hub.device('$id')?.name ?? '$id (removed)';

String _days(Object? days) {
  final d = (days as List?)?.cast<String>() ?? const [];
  if (d.isEmpty || d.length == 7) return '';
  if (d.length == 5 && !d.contains('sat') && !d.contains('sun')) return ' · weekdays';
  if (d.length == 2 && d.contains('sat') && d.contains('sun')) return ' · weekends';
  return ' · ${[for (final w in weekdays) if (d.contains(w)) weekdayLabels[w]].join(' ')}';
}

String _offset(Object? minutes) {
  final m = (minutes as num?)?.toInt() ?? 0;
  if (m == 0) return '';
  return m > 0 ? ' +$m min' : ' −${-m} min';
}

String showValue(Object? v) => v is String ? v : jsonEncode(v);

String describeTrigger(HubStore hub, Map<String, dynamic> t) => switch (t['type']) {
      'time' => 'At ${t['at']}${_days(t['days'])}',
      'sun' => 'At ${t['event']}${_offset(t['offset_min'])}${_days(t['days'])}',
      'state' => 'When ${deviceName(hub, t['device'])}: ${t['capability']}.${t['field']}'
          '${t.containsKey('to') ? ' becomes ${showValue(t['to'])}' : ' changes'}'
          '${t.containsKey('from') ? ' (from ${showValue(t['from'])})' : ''}',
      'threshold' => 'When ${deviceName(hub, t['device'])}: ${t['capability']}.${t['field']} goes '
          '${t.containsKey('above') ? 'above ${t['above']}' : 'below ${t['below']}'}',
      _ => jsonEncode(t),
    };

String describeCondition(HubStore hub, Map<String, dynamic> c) {
  switch (c['type']) {
    case 'time':
      final after = c['after'], before = c['before'];
      final span = after != null && before != null
          ? 'Between $after and $before'
          : after != null
              ? 'After $after'
              : before != null
                  ? 'Before $before'
                  : 'Any time';
      return '$span${_days(c['days'])}';
    case 'sun':
      return c['is'] == 'day' ? 'Only during the day' : 'Only at night';
    case 'state':
      return 'Only if ${deviceName(hub, c['device'])}: ${c['capability']}.${c['field']} is ${showValue(c['is'])}';
    case 'value':
      final parts = [if (c['above'] != null) 'above ${c['above']}', if (c['below'] != null) 'below ${c['below']}'];
      return 'Only if ${deviceName(hub, c['device'])}: ${c['capability']}.${c['field']} is ${parts.join(' and ')}';
  }
  return jsonEncode(c);
}

String describeStep(HubStore hub, Map<String, dynamic> s) {
  if (s['scene'] != null) {
    final scene = hub.scenes.where((x) => x['id'] == s['scene']).firstOrNull;
    return 'Run scene ${scene?['name'] ?? s['scene']}';
  }
  final who = deviceName(hub, s['device']);
  if (s['action'] != null) {
    final args = s['args'];
    final shown = args is Map && args.isNotEmpty ? ' ${args.values.map(showValue).join(' ')}' : '';
    return '$who: ${s['action']}$shown';
  }
  final v = s['value'];
  final text = switch (s['capability']) {
    'switch' => (v is Map && v['on'] == true) ? 'on' : 'off',
    'dimmer' => '${v is Map ? v['level'] : v} %',
    'lock' => v is Map && v['state'] == 'unlocked' ? 'UNLOCK' : 'lock',
    'cover' => v is Map ? '${v['position']} % open' : showValue(v),
    'climate' => v is Map ? '${v['mode']} ${v['target']} °C' : showValue(v),
    'color' => v is Map ? '${v['hex'] ?? '${v['kelvin']} K'}' : showValue(v),
    _ => showValue(v),
  };
  return '$who: ${s['capability']} → $text';
}

/// Every field of a device's state a trigger or condition can watch, as a
/// path ("on", "readings.temperature.value", "power_w") with its value now
/// -- so the editor can offer them instead of asking for typed paths.
Map<String, Map<String, Object?>> watchableFields(Device d) {
  final out = <String, Map<String, Object?>>{};
  d.capabilities.forEach((cap, state) {
    final fields = <String, Object?>{};
    void walk(String prefix, Object? v) {
      if (v is Map) {
        v.forEach((k, x) => walk(prefix.isEmpty ? '$k' : '$prefix.$k', x));
      } else if (v is bool || v is num || v is String || v == null) {
        fields[prefix] = v;
      }
      // Lists (inputs, palettes, buttons) aren't states to watch.
    }

    walk('', state);
    // What the device only describes about itself, not its state.
    fields.removeWhere((k, _) => const {'can_position', 'min', 'max', 'step', 'keyboard', 'learn'}.contains(k) || k.startsWith('brightness.'));
    if (fields.isNotEmpty) out[cap] = fields;
  });
  return out;
}

/// What a person typed as a value: JSON when it is JSON (true, 21.5,
/// "x"), the plain text otherwise.
Object? parseLoose(String text) {
  final t = text.trim();
  try {
    return jsonDecode(t);
  } catch (_) {
    return t;
  }
}

/// "07:30" from hour/minute.
String hhmm(int h, int m) => '${h.toString().padLeft(2, '0')}:${m.toString().padLeft(2, '0')}';
