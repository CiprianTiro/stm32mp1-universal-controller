import 'dart:async';
import 'dart:convert';

/// A pretend hub for demo mode (`flutter run --dart-define=DEMO=true`):
/// answers requests the way backend_daemon's ws.rs does, over a sample
/// house -- for trying the screens and taking screenshots without a DK2.
/// Never used in a normal build.
class DemoHub {
  final Map<String, Map<String, dynamic>> _devices = {
    for (final d in _sampleDevices) d['id'] as String: jsonDecode(jsonEncode(d)) as Map<String, dynamic>,
  };
  final List<Map<String, dynamic>> _scenes = jsonDecode(jsonEncode(_sampleScenes)).cast<Map<String, dynamic>>();
  final List<Map<String, dynamic>> _automations = jsonDecode(jsonEncode(_sampleAutomations)).cast<Map<String, dynamic>>();
  Map<String, dynamic> _settings = {'mode': 'dark', 'accent': 'sky', 'density': 'comfortable', 'time_zone': 'Europe/Bucharest', 'latitude': 44.43, 'longitude': 26.1};

  Future<Map<String, dynamic>> handle(Map<String, dynamic> m) async {
    // A little latency, like a real device answering.
    await Future<void>.delayed(const Duration(milliseconds: 120));
    switch (m['action']) {
      case 'list_devices':
        return {'type': 'devices', 'devices': _devices.values.toList()};
      case 'get_settings':
        return {'type': 'settings', ..._settings};
      case 'set_settings':
        _settings = {..._settings, ...m}..remove('action');
        return {'type': 'settings', ..._settings};
      case 'list_found':
        return {
          'type': 'found',
          'devices': [
            {'template': 'wled', 'name': 'WLED-Garden', 'address': '192.168.1.151', 'values': {}, 'identity': ''},
          ],
        };
      case 'list_automations':
        return {'type': 'automations', 'scenes': _scenes, 'automations': _automations};
      case 'command':
        final d = _devices[m['id']]!;
        final caps = d['capabilities'] as Map<String, dynamic>;
        final value = Map<String, dynamic>.from(m['value'] as Map)..remove('confirmed');
        caps[m['capability'] as String] = {...(caps[m['capability']] as Map<String, dynamic>), ...value};
        if (m['capability'] == 'switch' && caps['energy'] != null) {
          (caps['energy'] as Map)['power_w'] = value['on'] == true ? 1450.0 : 0.4;
        }
        return {'type': 'device', 'device': d};
      case 'device_action':
        final d = _devices[m['id']]!;
        final caps = d['capabilities'] as Map<String, dynamic>;
        if (m['capability'] == 'cover') {
          final c = caps['cover'] as Map<String, dynamic>;
          c['position'] = switch (m['name']) { 'open' => 100, 'close' => 0, _ => c['position'] };
        }
        return {'type': 'action_result', 'result': {}};
      case 'update_device_info':
        final d = _devices[m['id']]!;
        for (final k in ['name', 'room', 'favourite']) {
          if (m.containsKey(k)) d[k] = m[k];
        }
        return {'type': 'device', 'device': d};
      case 'run_scene':
        final scene = _scenes.firstWhere((s) => s['id'] == m['id']);
        for (final step in (scene['steps'] as List).cast<Map<String, dynamic>>()) {
          if (step['value'] != null) {
            await handle({'action': 'command', 'id': step['device'], 'capability': step['capability'], 'value': step['value']});
          }
        }
        return {'type': 'ack'};
      case 'run_automation':
        return {'type': 'ack'};
      case 'set_automation_enabled':
        _automations.firstWhere((a) => a['id'] == m['id'])['enabled'] = m['enabled'];
        return {'type': 'automations', 'scenes': _scenes, 'automations': _automations};
      case 'get_automation_log':
        final now = DateTime.now().millisecondsSinceEpoch ~/ 1000;
        return {
          'type': 'automation_log',
          'entries': [
            {'at': now - 7200, 'kind': 'automation', 'id': 'morning-coffee', 'name': 'Morning coffee', 'cause': '07:00', 'ok': true},
            {'at': now - 1800, 'kind': 'scene', 'id': 'movie-night', 'name': 'Movie night', 'cause': 'phone', 'ok': true},
            {'at': now - 600, 'kind': 'automation', 'id': 'evening', 'name': 'Evening lights', 'cause': 'sunset', 'ok': false, 'detail': 'Printer plug: not answering'},
          ],
        };
      case 'list_templates':
        return {
          'type': 'templates',
          'templates': [
            {'id': 'wled', 'name': 'WLED light', 'category': 'Lights', 'description': 'ESP32 LED controller with WLED', 'variants': [{'id': 'guided', 'label': 'Find it automatically'}, {'id': 'advanced', 'label': 'Enter the address'}], 'can_reauth': false, 'can_reconfigure': true},
            {'id': 'shelly-plug-gen3', 'name': 'Shelly Plug', 'category': 'Plugs', 'description': 'Metering smart plug', 'variants': [], 'can_reauth': false, 'can_reconfigure': true},
            {'id': 'lg-webos', 'name': 'LG TV', 'category': 'Media', 'description': 'webOS television', 'variants': [], 'can_reauth': true, 'can_reconfigure': true},
          ],
        };
      case 'webhook_url':
        return {'type': 'webhook', 'url': 'https://192.168.1.20:8443/hook/3f9c1e7a2b'};
    }
    return {'type': 'error', 'message': 'not available in demo mode'};
  }
}

const _sampleDevices = [
  {
    'id': 'ceiling', 'name': 'Ceiling light', 'room': 'Living room', 'template': 'wled', 'source': 'wled', 'online': 'online', 'favourite': true,
    'capabilities': {'switch': {'on': true}, 'dimmer': {'level': 70}, 'color': {'kelvin': 2700}},
  },
  {
    'id': 'tv', 'name': 'TV', 'room': 'Living room', 'template': 'lg-webos', 'source': 'lg-webos', 'online': 'online', 'favourite': true,
    'capabilities': {
      'switch': {'on': true},
      'media': {'volume': 12, 'muted': false, 'input': 'HDMI_1', 'inputs': [{'id': 'HDMI_1', 'label': 'PlayStation'}, {'id': 'HDMI_2', 'label': 'Apple TV'}, {'id': 'LIVE_TV', 'label': 'Live TV'}], 'app': {'id': 'netflix', 'label': 'Netflix'}},
      'remote': {'buttons': ['UP', 'DOWN', 'LEFT', 'RIGHT', 'OK', 'BACK', 'HOME', 'MENU', 'VOLUME_UP', 'VOLUME_DOWN', 'MUTE', 'CHANNEL_UP', 'CHANNEL_DOWN', 'PLAY', 'PAUSE'], 'keyboard': true},
    },
  },
  {
    'id': 'blinds', 'name': 'Blinds', 'room': 'Living room', 'template': 'cover', 'source': 'virtual',
    'capabilities': {'cover': {'position': 60, 'moving': 'stopped', 'can_position': true}},
  },
  {
    'id': 'ac', 'name': 'Air conditioner', 'room': 'Living room', 'template': 'climate', 'source': 'virtual', 'favourite': true,
    'capabilities': {'climate': {'mode': 'heat', 'target': 22.0, 'current': 21.4, 'fan': 'auto', 'modes': ['off', 'heat', 'cool', 'auto', 'dry'], 'fans': ['auto', 'low', 'medium', 'high'], 'min': 16, 'max': 30, 'step': 0.5}},
  },
  {
    'id': 'strip', 'name': 'LED strip', 'room': 'Kitchen', 'template': 'ir-light', 'source': 'ir-blaster', 'online': 'online',
    'capabilities': {
      'switch': {'on': false},
      'color': {'hex': '#FF8800', 'palette': ['#FF0000', '#FF8800', '#FFFF00', '#00FF00', '#00FFFF', '#0000FF', '#FF00FF', '#FFFFFF']},
      'remote': {'buttons': ['Brighter', 'Dimmer'], 'learn': true, 'brightness': {'up': 'Brighter', 'down': 'Dimmer'}},
    },
  },
  {
    'id': 'coffee', 'name': 'Coffee machine', 'room': 'Kitchen', 'template': 'shelly-plug-gen3', 'source': 'shelly', 'online': 'online',
    'capabilities': {'switch': {'on': false}, 'energy': {'power_w': 0.4, 'energy_kwh': 12.84, 'voltage_v': 231.2}},
  },
  {
    'id': 'bedside', 'name': 'Bedside lamp', 'room': 'Bedroom', 'template': 'wled', 'source': 'wled', 'online': 'online',
    'capabilities': {'switch': {'on': false}, 'dimmer': {'level': 30}},
  },
  {
    'id': 'climate-sensor', 'name': 'Thermometer', 'room': 'Bedroom', 'template': 'sensor', 'source': 'virtual',
    'capabilities': {'sensor': {'readings': {'temperature': {'value': 20.8, 'unit': '°C'}, 'humidity': {'value': 48, 'unit': '%'}}}},
  },
  {
    'id': 'door', 'name': 'Front door', 'room': 'Hall', 'template': 'lock', 'source': 'virtual',
    'capabilities': {'lock': {'state': 'locked'}},
  },
  {
    'id': 'desk', 'name': 'Desk lamp', 'room': 'Office', 'template': 'wled', 'source': 'wled', 'online': 'online',
    'capabilities': {'switch': {'on': true}, 'dimmer': {'level': 40}, 'color': {'hex': '#38BDF8'}},
  },
  {
    'id': 'printer', 'name': 'Printer plug', 'room': 'Office', 'template': 'shelly-plug-gen3', 'source': 'shelly', 'online': 'offline',
    'capabilities': {'switch': {'on': false}, 'energy': {'power_w': 0}},
  },
];

const _sampleScenes = [
  {
    'id': 'movie-night', 'name': 'Movie night',
    'steps': [
      {'device': 'ceiling', 'capability': 'dimmer', 'value': {'level': 15}},
      {'device': 'tv', 'capability': 'switch', 'value': {'on': true}},
      {'device': 'blinds', 'capability': 'cover', 'value': {'position': 0}},
    ],
  },
  {
    'id': 'good-morning', 'name': 'Good morning',
    'steps': [
      {'device': 'blinds', 'capability': 'cover', 'value': {'position': 100}},
      {'device': 'coffee', 'capability': 'switch', 'value': {'on': true}},
    ],
  },
  {
    'id': 'away', 'name': 'Away',
    'steps': [
      {'device': 'ceiling', 'capability': 'switch', 'value': {'on': false}},
      {'device': 'desk', 'capability': 'switch', 'value': {'on': false}},
      {'device': 'door', 'capability': 'lock', 'value': {'state': 'locked'}},
    ],
  },
  {
    'id': 'bedtime', 'name': 'Bedtime',
    'steps': [
      {'device': 'bedside', 'capability': 'dimmer', 'value': {'level': 10}},
      {'device': 'ceiling', 'capability': 'switch', 'value': {'on': false}},
    ],
  },
];

const _sampleAutomations = [
  {
    'id': 'evening', 'name': 'Evening lights', 'enabled': true,
    'triggers': [{'type': 'sun', 'event': 'sunset', 'offset_min': -15}],
    'conditions': [],
    'steps': [{'device': 'ceiling', 'capability': 'switch', 'value': {'on': true}}],
  },
  {
    'id': 'morning-coffee', 'name': 'Morning coffee', 'enabled': true,
    'triggers': [{'type': 'time', 'at': '07:00', 'days': ['mon', 'tue', 'wed', 'thu', 'fri']}],
    'conditions': [],
    'steps': [{'scene': 'good-morning'}],
  },
  {
    'id': 'tv-blinds', 'name': 'Close blinds for the TV', 'enabled': false,
    'triggers': [{'type': 'state', 'device': 'tv', 'capability': 'switch', 'field': 'on', 'to': true}],
    'conditions': [{'type': 'sun', 'is': 'day'}],
    'steps': [{'device': 'blinds', 'capability': 'cover', 'value': {'position': 20}}],
  },
];
