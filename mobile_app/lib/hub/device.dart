/// A device as the hub sends it (backend_daemon's device.rs, protocol v2):
///
///   {"id": "lamp-1", "name": "Living room lamp", "room": "Living room",
///    "template": "dimmable-light", "source": "wled", "online": "online",
///    "capabilities": {"switch": {"on": true}, "dimmer": {"level": 80}}}
///
/// The capabilities stay the hub's raw JSON maps: a screen is built from
/// them alone (switch -> toggle, dimmer -> slider), and a capability this
/// app version doesn't know yet (a newer hub's "vacuum") still arrives
/// whole and is shown read-only instead of being dropped.
class Device {
  Device({
    required this.id,
    required this.name,
    required this.room,
    required this.template,
    required this.source,
    required this.online,
    required this.lastSeen,
    required this.favourite,
    required this.capabilities,
    this.config = const {},
  });

  final String id;
  final String name;

  /// "" = not assigned to a room.
  final String room;
  final String template;

  /// The adapter that runs it ("wled", "lg-webos", ...) or "virtual".
  final String source;

  /// "online" / "offline" / "unauthorized"; null = not known (a virtual
  /// device, or an adapter that hasn't connected yet).
  final String? online;

  /// Battery devices (#72): when the hub last heard from them.
  final DateTime? lastSeen;
  final bool favourite;
  final Map<String, Map<String, dynamic>> capabilities;

  /// The adapter's plain settings ({"host": ...}; never secrets). The
  /// screens read the "powered by" link from it (#99, power_link.rs).
  final Map<String, String> config;

  factory Device.fromJson(Map<String, dynamic> j) {
    final caps = <String, Map<String, dynamic>>{};
    final raw = j['capabilities'];
    if (raw is Map) {
      raw.forEach((k, v) {
        if (k is String && v is Map) caps[k] = Map<String, dynamic>.from(v);
      });
    }
    final seen = j['last_seen'];
    final config = <String, String>{};
    final rawConfig = j['config'];
    if (rawConfig is Map) {
      rawConfig.forEach((k, v) {
        if (k is String && v is String) config[k] = v;
      });
    }
    return Device(
      id: j['id'] as String,
      name: j['name'] as String? ?? j['id'] as String,
      room: j['room'] as String? ?? '',
      template: j['template'] as String? ?? '',
      source: j['source'] as String? ?? 'virtual',
      online: j['online'] as String?,
      lastSeen: seen is int ? DateTime.fromMillisecondsSinceEpoch(seen * 1000) : null,
      favourite: j['favourite'] as bool? ?? false,
      capabilities: caps,
      config: config,
    );
  }

  bool has(String capability) => capabilities.containsKey(capability);
  Map<String, dynamic>? cap(String capability) => capabilities[capability];

  /// Anything but online (or unknown) is shown greyed out, with the reason.
  bool get reachable => online == null || online == 'online';

  bool? get isOn => cap('switch')?['on'] as bool?;
  int? get level => (cap('dimmer')?['level'] as num?)?.toInt();
}

/// The capabilities this app version can show controls for (device.rs's
/// CAPABILITY_NAMES as of protocol v2). Others get a read-only card.
const knownCapabilities = {
  'switch',
  'dimmer',
  'color',
  'sensor',
  'media',
  'remote',
  'cover',
  'climate',
  'lock',
  'energy',
  // #74
  'vacuum',
  'camera',
  'options',
};

/// The buttons a screen lays out as a real remote (device.rs's
/// REMOTE_BUTTONS); a learning remote's other buttons are a labelled grid.
const remoteLayoutButtons = {
  'UP', 'DOWN', 'LEFT', 'RIGHT', 'OK', 'BACK', 'HOME', 'MENU', 'EXIT', 'INFO',
  'VOLUME_UP', 'VOLUME_DOWN', 'MUTE', 'CHANNEL_UP', 'CHANNEL_DOWN',
  'PLAY', 'PAUSE', 'STOP', 'REWIND', 'FAST_FORWARD',
  '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
  'RED', 'GREEN', 'YELLOW', 'BLUE',
};
