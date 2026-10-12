part of 'capability_controls.dart';

// The capabilities #74 added to the hub (device.rs): a robot vacuum, a
// camera's pictures and a device's own options -- plus a TV's apps and
// channels (#44). Like the rest of this file: drawn from the capability's
// JSON alone, steered with its actions (hub.action), no code per vendor.

// ---- options ----

/// A device's own settings (#74, device.rs's Options): a plug's LED, a
/// camera's privacy mode, motion detection, night vision... A plain one is
/// a switch; one with `choices` is a row of chips. Changed with the action
/// set {"id", "on"} / {"id", "value"}.
class _OptionsControl extends StatelessWidget {
  const _OptionsControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final options = ((device.cap('options')!['options'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final sp = Spacing.of(context);
    void set(Map<String, dynamic> args) => runRequest(context, () => hub.action(device.id, 'options', 'set', args));
    return _Section(
      title: 'Settings',
      child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        for (final o in options)
          if (((o['choices'] as List?) ?? const []).isEmpty)
            SwitchListTile(
              title: Text(o['name'] as String? ?? o['id'] as String),
              value: o['on'] as bool? ?? false,
              contentPadding: EdgeInsets.zero,
              onChanged: (v) => set({'id': o['id'], 'on': v}),
            )
          else
            Padding(
              padding: EdgeInsets.symmetric(vertical: sp.s),
              child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                Text(o['name'] as String? ?? o['id'] as String),
                SizedBox(height: sp.xs),
                Wrap(spacing: sp.s, runSpacing: sp.s, children: [
                  for (final c in (o['choices'] as List).cast<String>())
                    ChoiceChip(
                      label: Text(_humanize(c)),
                      selected: c == o['value'],
                      onSelected: (_) => set({'id': o['id'], 'value': c}),
                    ),
                ]),
              ]),
            ),
      ]),
    );
  }
}

// ---- vacuum ----

/// A robot vacuum (#45/#74, device.rs's Vacuum): what it's doing, the
/// buttons, its suction / water / mop route, cleaning chosen rooms, its
/// map, and the wearing parts. Everything only comes FROM the vacuum; it's
/// steered with the actions (start, pause, dock, set_fan, clean_rooms...).
class _VacuumControl extends StatefulWidget {
  const _VacuumControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_VacuumControl> createState() => _VacuumControlState();
}

class _VacuumControlState extends State<_VacuumControl> {
  final Set<int> _rooms = {};
  int _repeat = 1;
  bool _showMap = false;

  void _act(String name, [Map<String, dynamic>? args]) =>
      runRequest(context, () => widget.hub.action(widget.device.id, 'vacuum', name, args));

  Future<void> _resetPart(Map<String, dynamic> part) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Reset “${part['name']}”?'),
        content: const Text('Only after replacing or cleaning it: its life starts again at 100 %.'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Reset')),
        ],
      ),
    );
    if (ok == true && mounted) _act('reset_part', {'part': part['id']});
  }

  @override
  Widget build(BuildContext context) {
    final v = widget.device.cap('vacuum')!;
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final state = v['state'] as String? ?? 'unknown';
    final battery = (v['battery'] as num?)?.toInt();
    final error = v['error'] as String? ?? '';
    final detail = v['detail'] as String? ?? '';
    final rooms = ((v['rooms'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final parts = ((v['parts'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final totals = v['totals'] as Map<String, dynamic>?;
    final cleaning = state == 'cleaning';

    // A mode the vacuum offers, as chips: {"fan": "balanced"} etc.
    Widget modes(String title, String key, String listKey, String action) {
      final choices = ((v[listKey] as List?) ?? const []).cast<String>();
      if (choices.isEmpty) return const SizedBox.shrink();
      return Padding(
        padding: EdgeInsets.only(top: sp.m),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(title, style: theme.textTheme.bodySmall),
          SizedBox(height: sp.xs),
          Wrap(spacing: sp.s, runSpacing: sp.s, children: [
            for (final c in choices)
              ChoiceChip(label: Text(_humanize(c)), selected: c == v[key], onSelected: (_) => _act(action, {key: c})),
          ]),
        ]),
      );
    }

    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      _Section(
        title: 'Vacuum',
        trailing: battery == null
            ? null
            : Row(mainAxisSize: MainAxisSize.min, children: [
                Icon(battery > 20 ? Icons.battery_full : Icons.battery_alert, size: 18),
                Text(' $battery %'),
              ]),
        child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text(
            [_vacuumStates[state] ?? _humanize(state), if (detail.isNotEmpty && detail != state) detail].join(' · '),
            style: theme.textTheme.titleMedium,
          ),
          if (error.isNotEmpty) ...[
            SizedBox(height: sp.xs),
            Text(error, style: TextStyle(color: theme.colorScheme.error)),
          ],
          if (v['area_m2'] != null || v['minutes'] != null) ...[
            SizedBox(height: sp.xs),
            Text(
              [
                if (v['area_m2'] != null) '${_num(v['area_m2'])} m²',
                if (v['minutes'] != null) '${v['minutes']} min',
              ].join(' · '),
              style: theme.textTheme.bodySmall,
            ),
          ],
          SizedBox(height: sp.m),
          Row(children: [
            Expanded(
              child: FilledButton.icon(
                onPressed: () => _act(cleaning ? 'pause' : 'start'),
                icon: Icon(cleaning ? Icons.pause : Icons.play_arrow),
                label: Text(cleaning ? 'Pause' : 'Start'),
              ),
            ),
            SizedBox(width: sp.s),
            Expanded(
              child: OutlinedButton.icon(onPressed: () => _act('dock'), icon: const Icon(Icons.home_outlined), label: const Text('Dock')),
            ),
          ]),
          Row(mainAxisAlignment: MainAxisAlignment.center, children: [
            TextButton.icon(onPressed: () => _act('stop'), icon: const Icon(Icons.stop), label: const Text('Stop')),
            TextButton.icon(onPressed: () => _act('locate'), icon: const Icon(Icons.volume_up_outlined), label: const Text('Find it')),
          ]),
          modes('Suction', 'fan', 'fans', 'set_fan'),
          modes('Mopping water', 'water', 'waters', 'set_water'),
          modes('Mop route', 'mop', 'mops', 'set_mop'),
          if (v['quiet_hours'] != null) ...[
            SizedBox(height: sp.m),
            _ValueRow(label: 'Do not disturb', value: v['quiet_hours'] as String),
          ],
        ]),
      ),
      if (rooms.isNotEmpty) ...[
        SizedBox(height: sp.l),
        _Section(
          title: 'Clean rooms',
          child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
            Wrap(spacing: sp.s, runSpacing: sp.s, children: [
              for (final r in rooms)
                FilterChip(
                  label: Text(r['name'] as String? ?? '${r['id']}'),
                  selected: _rooms.contains(r['id']),
                  onSelected: (on) => setState(() => on ? _rooms.add(r['id'] as int) : _rooms.remove(r['id'])),
                ),
            ]),
            SizedBox(height: sp.m),
            Row(children: [
              const Text('Times'),
              SizedBox(width: sp.s),
              SegmentedButton<int>(
                segments: const [
                  ButtonSegment(value: 1, label: Text('1')),
                  ButtonSegment(value: 2, label: Text('2')),
                  ButtonSegment(value: 3, label: Text('3')),
                ],
                selected: {_repeat},
                onSelectionChanged: (s) => setState(() => _repeat = s.first),
              ),
              const Spacer(),
              FilledButton(
                onPressed: _rooms.isEmpty ? null : () => _act('clean_rooms', {'rooms': _rooms.toList(), 'repeat': _repeat}),
                child: const Text('Clean'),
              ),
            ]),
          ]),
        ),
      ],
      if (v['map'] == true) ...[
        SizedBox(height: sp.l),
        _Section(
          title: 'Map',
          trailing: TextButton(
            onPressed: () => setState(() => _showMap = !_showMap),
            child: Text(_showMap ? 'Hide' : 'Show'),
          ),
          child: _showMap ? _VacuumMap(device: widget.device, hub: widget.hub) : const SizedBox.shrink(),
        ),
      ],
      if (parts.isNotEmpty || totals != null) ...[
        SizedBox(height: sp.l),
        _Section(
          title: 'Maintenance',
          child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
            for (final p in parts)
              ListTile(
                contentPadding: EdgeInsets.zero,
                title: Text(p['name'] as String? ?? p['id'] as String),
                subtitle: Padding(
                  padding: EdgeInsets.only(top: sp.xs),
                  child: LinearProgressIndicator(
                    value: ((p['left'] as num?) ?? 0) / 100,
                    color: ((p['left'] as num?) ?? 0) < 10 ? theme.colorScheme.error : null,
                  ),
                ),
                trailing: Row(mainAxisSize: MainAxisSize.min, children: [
                  Text('${p['left']} %'),
                  IconButton(onPressed: () => _resetPart(p), icon: const Icon(Icons.restart_alt), tooltip: 'Reset after replacing'),
                ]),
              ),
            if (totals != null) ...[
              SizedBox(height: sp.s),
              _ValueRow(label: 'Cleanings', value: '${totals['cleanings']}'),
              _ValueRow(label: 'Area in total', value: '${_num(totals['area_m2'], 0)} m²'),
              _ValueRow(label: 'Hours in total', value: _num(totals['hours'])),
            ],
          ]),
        ),
      ],
    ]);
  }
}

const _vacuumStates = {
  'docked': 'On its dock',
  'cleaning': 'Cleaning',
  'returning': 'Going back to the dock',
  'paused': 'Paused',
  'idle': 'Idle',
  'error': 'Needs help',
  'unknown': 'Not known yet',
};

/// The vacuum's map (the action "map", roborock_map.rs's Map::to_json):
/// one class per square -- 0 outside, 1 wall, 2 floor, 10+n room n --
/// run-length packed and base64, plus the dock, the vacuum, the path it
/// drove, no-go areas and virtual walls in the same squares (y down).
/// Fetched when shown, again every 10 s while it cleans.
class _VacuumMap extends StatefulWidget {
  const _VacuumMap({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_VacuumMap> createState() => _VacuumMapState();
}

class _VacuumMapState extends State<_VacuumMap> {
  VacuumMapData? _map;
  ui.Image? _image;
  String? _error;
  Timer? _timer;
  bool _loading = false;

  @override
  void initState() {
    super.initState();
    _load();
    _timer = Timer.periodic(const Duration(seconds: 10), (_) {
      if (widget.device.cap('vacuum')?['state'] == 'cleaning') _load();
    });
  }

  @override
  void dispose() {
    _timer?.cancel();
    _image?.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    if (_loading) return;
    _loading = true;
    // The theme's colours, read before waiting for the hub.
    final scheme = Theme.of(context).colorScheme;
    try {
      // A map takes the hub a few cloud requests (20 s each at most).
      final result = await widget.hub.action(widget.device.id, 'vacuum', 'map', null, const Duration(seconds: 90));
      final map = VacuumMapData.fromJson(result);
      if (map == null) throw const FormatException('not a map');
      final image = await map.toImage(scheme);
      if (!mounted) return image.dispose();
      setState(() {
        _image?.dispose();
        _image = image;
        _map = map;
        _error = null;
      });
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    } finally {
      _loading = false;
    }
  }

  @override
  Widget build(BuildContext context) {
    final image = _image;
    final map = _map;
    if (image == null || map == null) {
      return Padding(
        padding: EdgeInsets.all(Spacing.of(context).l),
        child: Center(child: _error == null ? const CircularProgressIndicator() : Text(_error!)),
      );
    }
    return AspectRatio(
      aspectRatio: map.width / map.height,
      child: InteractiveViewer(
        maxScale: 6,
        child: CustomPaint(painter: _VacuumMapPainter(map, image, Theme.of(context).colorScheme)),
      ),
    );
  }
}

/// A decoded vacuum map. Public (and pure) for the tests.
class VacuumMapData {
  VacuumMapData({
    required this.width,
    required this.height,
    required this.classes,
    this.dock,
    this.robot,
    this.path = const [],
    this.noGo = const [],
    this.walls = const [],
  });

  final int width, height;

  /// One byte per square, row by row.
  final Uint8List classes;
  final Offset? dock, robot;
  final List<Offset> path;
  final List<List<Offset>> noGo, walls;

  static VacuumMapData? fromJson(Object? j) {
    if (j is! Map) return null;
    final w = j['width'], h = j['height'], pixels = j['pixels'];
    if (w is! int || h is! int || pixels is! String || w <= 0 || h <= 0 || w > 4096 || h > 4096) return null;
    final classes = unpackRuns(pixels, w * h);
    if (classes == null) return null;
    Offset? point(Object? p) => p is List && p.length >= 2 && p[0] is num && p[1] is num
        ? Offset((p[0] as num).toDouble(), (p[1] as num).toDouble())
        : null;
    List<Offset> points(Object? list) => [for (final p in (list is List ? list : const [])) ?point(p)];
    return VacuumMapData(
      width: w,
      height: h,
      classes: classes,
      dock: point(j['dock']),
      robot: point(j['robot']),
      path: points(j['path']),
      noGo: [for (final a in (j['no_go'] is List ? j['no_go'] as List : const [])) points(a)],
      walls: [for (final a in (j['walls'] is List ? j['walls'] as List : const [])) points(a)],
    );
  }

  /// base64 of [value, count]... -> exactly [size] bytes, or null.
  static Uint8List? unpackRuns(String text, int size) {
    final Uint8List runs;
    try {
      runs = base64Decode(text);
    } on FormatException {
      return null;
    }
    final out = Uint8List(size);
    var n = 0;
    for (var i = 0; i + 1 < runs.length; i += 2) {
      final count = runs[i + 1];
      if (n + count > size) return null;
      out.fillRange(n, n + count, runs[i]);
      n += count;
    }
    return n == size ? out : null;
  }

  /// The floor plan as a picture, one pixel per square, in the theme's
  /// colours: rooms each in a soft tint of their own (as on the hub).
  Future<ui.Image> toImage(ColorScheme scheme) {
    final floor = scheme.surfaceContainerHighest;
    final wall = scheme.onSurfaceVariant;
    final rgba = Uint8List(width * height * 4);
    for (var i = 0; i < classes.length; i++) {
      final c = classes[i];
      final Color color = switch (c) {
        0 => const Color(0x00000000),
        1 => wall,
        2 => floor,
        _ => Color.lerp(_roomTints[(c - 10) % _roomTints.length], floor, 0.5)!,
      };
      rgba[i * 4] = (color.r * 255).round();
      rgba[i * 4 + 1] = (color.g * 255).round();
      rgba[i * 4 + 2] = (color.b * 255).round();
      rgba[i * 4 + 3] = (color.a * 255).round();
    }
    final done = Completer<ui.Image>();
    ui.decodeImageFromPixels(rgba, width, height, ui.PixelFormat.rgba8888, done.complete);
    return done.future;
  }
}

const _roomTints = [
  Color(0xFF6EA0DC),
  Color(0xFF78BE8C),
  Color(0xFFDCAA64),
  Color(0xFFBE82C8),
  Color(0xFF64BEBE),
  Color(0xFFDC8282),
  Color(0xFFAAAA6E),
  Color(0xFF8C96DC),
];

class _VacuumMapPainter extends CustomPainter {
  _VacuumMapPainter(this.map, this.image, this.scheme);
  final VacuumMapData map;
  final ui.Image image;
  final ColorScheme scheme;

  @override
  void paint(Canvas canvas, Size size) {
    final scale = size.width / map.width;
    // Squares stay sharp when zoomed: no smoothing.
    canvas.drawImageRect(
      image,
      Rect.fromLTWH(0, 0, map.width.toDouble(), map.height.toDouble()),
      Offset.zero & size,
      Paint()..filterQuality = FilterQuality.none,
    );
    Offset at(Offset p) => Offset((p.dx + 0.5) * scale, (p.dy + 0.5) * scale);
    final line = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = (scale / 3).clamp(0.8, 3)
      ..strokeCap = StrokeCap.round;
    // No-go areas (outlined) and virtual walls.
    line.color = scheme.error;
    for (final area in map.noGo) {
      if (area.length > 2) canvas.drawPath(Path()..addPolygon(area.map(at).toList(), true), line);
    }
    for (final w in map.walls) {
      if (w.length == 2) canvas.drawLine(at(w[0]), at(w[1]), line..strokeWidth = (scale / 2).clamp(1, 4));
    }
    // The path it drove.
    if (map.path.length > 1) {
      line
        ..color = scheme.onSurface.withValues(alpha: 0.5)
        ..strokeWidth = (scale / 3).clamp(0.8, 3);
      canvas.drawPath(Path()..addPolygon(map.path.map(at).toList(), false), line);
    }
    // The dock, then the vacuum on top (in a ring: visible on any colour).
    final r = (scale * 3).clamp(4.0, 12.0);
    if (map.dock != null) canvas.drawCircle(at(map.dock!), r, Paint()..color = const Color(0xFF16A34A));
    if (map.robot != null) {
      canvas.drawCircle(at(map.robot!), r * 1.3, Paint()..color = scheme.surface);
      canvas.drawCircle(at(map.robot!), r, Paint()..color = scheme.primary);
    }
  }

  @override
  bool shouldRepaint(_VacuumMapPainter old) => old.image != image || old.scheme != scheme;
}

// ---- camera ----

/// A camera's pictures (#43/#74, device.rs's Camera): a recent picture
/// from the hub (the action snapshot -> {"jpeg": base64}), refreshed every
/// few seconds while the page is open; pan / tilt and saved positions.
/// No video goes through the hub: the full stream is the phone's job.
class _CameraControl extends StatefulWidget {
  const _CameraControl({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_CameraControl> createState() => _CameraControlState();
}

class _CameraControlState extends State<_CameraControl> {
  Uint8List? _jpeg;
  String? _error;
  Timer? _timer;
  bool _loading = false;

  /// Each picture is a key frame of the camera's stream (cheap for the
  /// hub: ffmpeg skips everything else) -- one every few seconds.
  static const _every = Duration(seconds: 3);

  bool get _hasSnapshot => widget.device.cap('camera')?['snapshot'] == true;

  @override
  void initState() {
    super.initState();
    if (_hasSnapshot) {
      _load();
      _timer = Timer.periodic(_every, (_) => _load());
    }
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  Future<void> _load() async {
    // A picture still on its way (a slow camera): don't pile up requests.
    if (_loading || !widget.device.reachable) return;
    _loading = true;
    try {
      final result = await widget.hub.action(widget.device.id, 'camera', 'snapshot');
      final jpeg = result is Map && result['jpeg'] is String ? base64Decode(result['jpeg'] as String) : null;
      if (mounted && jpeg != null) {
        setState(() {
          _jpeg = jpeg;
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    } finally {
      _loading = false;
    }
  }

  Future<void> _streamAddress() async {
    final messenger = ScaffoldMessenger.of(context);
    try {
      final result = await widget.hub.action(widget.device.id, 'camera', 'stream', {'quality': 'hd'});
      final url = result is Map ? result['url'] as String? : null;
      if (url == null || !mounted) return;
      await showDialog<void>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Video stream'),
          content: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
            const Text('Open it in a video player app (e.g. VLC: Open network stream). It contains the camera\'s '
                'stream password: keep it to yourself.'),
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

  void _move(double pan, double tilt) =>
      runRequest(context, () => widget.hub.action(widget.device.id, 'camera', 'move', {'pan': pan, 'tilt': tilt}));

  @override
  Widget build(BuildContext context) {
    final c = widget.device.cap('camera')!;
    final sp = Spacing.of(context);
    final presets = ((c['presets'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final jpeg = _jpeg;
    return _Section(
      title: 'Camera',
      trailing: c['stream'] == true
          ? TextButton.icon(onPressed: _streamAddress, icon: const Icon(Icons.videocam_outlined), label: const Text('Video'))
          : null,
      child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        if (!_hasSnapshot)
          Text('No pictures: set up its video in “Change settings” (the camera\'s stream account).',
              style: Theme.of(context).textTheme.bodySmall)
        else
          AspectRatio(
            aspectRatio: 16 / 9,
            child: ClipRRect(
              borderRadius: BorderRadius.circular(8),
              child: ColoredBox(
                color: Colors.black,
                child: jpeg != null
                    ? Image.memory(jpeg, gaplessPlayback: true, fit: BoxFit.contain)
                    : Center(
                        child: _error == null
                            ? const CircularProgressIndicator()
                            : Padding(
                                padding: EdgeInsets.all(sp.l),
                                child: Text(_error!, style: const TextStyle(color: Colors.white70), textAlign: TextAlign.center),
                              ),
                      ),
              ),
            ),
          ),
        if (c['ptz'] == true) ...[
          SizedBox(height: sp.m),
          // A short nudge per press: half speed, the camera stops by itself.
          Column(children: [
            IconButton.filledTonal(onPressed: () => _move(0, 0.5), icon: const Icon(Icons.keyboard_arrow_up), tooltip: 'Up'),
            Row(mainAxisAlignment: MainAxisAlignment.center, children: [
              IconButton.filledTonal(onPressed: () => _move(-0.5, 0), icon: const Icon(Icons.keyboard_arrow_left), tooltip: 'Left'),
              SizedBox(width: sp.xl * 2),
              IconButton.filledTonal(onPressed: () => _move(0.5, 0), icon: const Icon(Icons.keyboard_arrow_right), tooltip: 'Right'),
            ]),
            IconButton.filledTonal(onPressed: () => _move(0, -0.5), icon: const Icon(Icons.keyboard_arrow_down), tooltip: 'Down'),
          ]),
          if (presets.isNotEmpty)
            Wrap(spacing: sp.s, runSpacing: sp.s, children: [
              for (final p in presets)
                ActionChip(
                  label: Text(p['name'] as String? ?? p['token'] as String),
                  onPressed: () => runRequest(
                      context, () => widget.hub.action(widget.device.id, 'camera', 'preset', {'token': p['token']})),
                ),
            ]),
        ],
      ]),
    );
  }
}

// ---- a TV's apps and channels (#44) ----

/// "Apps" and "Channels" under a TV's sound and source: lists the hub
/// fetches from the TV when opened (the actions apps / channels), searched
/// and paged by the hub -- a satellite TV has thousands of channels.
class _MediaLists extends StatelessWidget {
  const _MediaLists({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Row(children: [
      Expanded(
        child: OutlinedButton.icon(
          onPressed: () => showModalBottomSheet<void>(
            context: context,
            isScrollControlled: true,
            builder: (_) => _AppsSheet(device: device, hub: hub),
          ),
          icon: const Icon(Icons.apps),
          label: const Text('Apps'),
        ),
      ),
      SizedBox(width: sp.s),
      Expanded(
        child: OutlinedButton.icon(
          onPressed: () => showModalBottomSheet<void>(
            context: context,
            isScrollControlled: true,
            builder: (_) => _ChannelsSheet(device: device, hub: hub),
          ),
          icon: const Icon(Icons.live_tv),
          label: const Text('Channels'),
        ),
      ),
    ]);
  }
}

class _AppsSheet extends StatefulWidget {
  const _AppsSheet({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_AppsSheet> createState() => _AppsSheetState();
}

class _AppsSheetState extends State<_AppsSheet> {
  List<Map<String, dynamic>>? _apps;
  String? _error;

  @override
  void initState() {
    super.initState();
    widget.hub.action(widget.device.id, 'media', 'apps').then((r) {
      if (mounted) setState(() => _apps = ((r is Map ? r['apps'] : null) as List? ?? const []).cast<Map<String, dynamic>>());
    }, onError: (Object e) {
      if (mounted) setState(() => _error = '$e');
    });
  }

  @override
  Widget build(BuildContext context) {
    final apps = _apps;
    return _Sheet(
      title: 'Apps',
      child: apps == null
          ? Center(child: _error == null ? const CircularProgressIndicator() : Text(_error!))
          : ListView(children: [
              for (final a in apps)
                ListTile(
                  title: Text(a['label'] as String? ?? a['id'] as String),
                  onTap: () {
                    Navigator.pop(context);
                    runRequest(context, () => widget.hub.action(widget.device.id, 'media', 'launch', {'app': a['id']}));
                  },
                ),
            ]),
    );
  }
}

class _ChannelsSheet extends StatefulWidget {
  const _ChannelsSheet({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_ChannelsSheet> createState() => _ChannelsSheetState();
}

class _ChannelsSheetState extends State<_ChannelsSheet> {
  static const _page = 100;
  final _query = TextEditingController();
  final List<Map<String, dynamic>> _channels = [];
  int _total = 0;
  bool _loading = false;
  String? _error;
  Timer? _typing;

  @override
  void initState() {
    super.initState();
    _load(reset: true);
  }

  @override
  void dispose() {
    _typing?.cancel();
    _query.dispose();
    super.dispose();
  }

  Future<void> _load({required bool reset}) async {
    final query = _query.text.trim();
    setState(() {
      _loading = true;
      if (reset) _channels.clear();
    });
    try {
      final r = await widget.hub.action(widget.device.id, 'media', 'channels', {
        if (query.isNotEmpty) 'query': query,
        'offset': _channels.length,
        'limit': _page,
      });
      // An answer for an older search (typing on): dropped.
      if (!mounted || query != _query.text.trim()) return;
      setState(() {
        _channels.addAll(((r is Map ? r['channels'] : null) as List? ?? const []).cast<Map<String, dynamic>>());
        _total = (r is Map ? r['total'] as int? : null) ?? _channels.length;
        _error = null;
      });
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return _Sheet(
      title: 'Channels',
      child: Column(children: [
        TextField(
          controller: _query,
          decoration: const InputDecoration(prefixIcon: Icon(Icons.search), hintText: 'Number or name'),
          onChanged: (_) {
            _typing?.cancel();
            _typing = Timer(const Duration(milliseconds: 300), () => _load(reset: true));
          },
        ),
        SizedBox(height: sp.s),
        if (_error != null) Text(_error!),
        Expanded(
          child: ListView.builder(
            itemCount: _channels.length + (_channels.length < _total ? 1 : 0),
            itemBuilder: (context, i) {
              if (i == _channels.length) {
                return TextButton(
                  onPressed: _loading ? null : () => _load(reset: false),
                  child: Text('Show more (${_total - _channels.length})'),
                );
              }
              final c = _channels[i];
              return ListTile(
                leading: SizedBox(width: 48, child: Text(c['number'] as String? ?? '', textAlign: TextAlign.end)),
                title: Text(c['name'] as String? ?? ''),
                onTap: () {
                  Navigator.pop(context);
                  runRequest(context, () => widget.hub.action(widget.device.id, 'media', 'tune', {'channel': c['id']}));
                },
              );
            },
          ),
        ),
        if (_loading && _channels.isEmpty) const LinearProgressIndicator(),
      ]),
    );
  }
}

/// A bottom sheet with a title, most of the screen high.
class _Sheet extends StatelessWidget {
  const _Sheet({required this.title, required this.child});
  final String title;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return SafeArea(
      child: SizedBox(
        height: MediaQuery.of(context).size.height * 0.8,
        child: Padding(
          padding: EdgeInsets.fromLTRB(sp.l, sp.l, sp.l, MediaQuery.of(context).viewInsets.bottom),
          child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
            Text(title, style: Theme.of(context).textTheme.titleLarge),
            SizedBox(height: sp.m),
            Expanded(child: child),
          ]),
        ),
      ),
    );
  }
}
