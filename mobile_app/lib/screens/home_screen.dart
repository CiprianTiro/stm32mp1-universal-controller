import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../hub/device.dart';
import '../hub/hub_address.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import '../widgets/capability_controls.dart';
import 'add_device_screen.dart';
import 'device_screen.dart';
import 'hub_settings_screen.dart';

/// Home, in the manner of Apple Home: a large title, the house at a glance
/// (status chips), scenes one tap away, and the devices as tiles that light
/// up when on -- filtered by room. Live: the hub pushes every change.
class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key, required this.hub});

  final HubStore hub;

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

/// The room filter's two special entries (rooms are their own names).
const _all = '\u0000all';
const _favourites = '\u0000fav';

class _HomeScreenState extends State<HomeScreen> {
  String _room = _all;

  HubStore get hub => widget.hub;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final devices = hub.devices..sort((a, b) => a.name.toLowerCase().compareTo(b.name.toLowerCase()));
    final rooms = hub.rooms;
    if (_room != _all && _room != _favourites && !rooms.contains(_room)) _room = _all;
    final hasFavourites = devices.any((d) => d.favourite);
    final pad = EdgeInsets.symmetric(horizontal: sp.xl);

    // Sections of tiles: everything grouped by room, or one filter's tiles.
    final sections = <(String?, List<Device>)>[
      if (_room == _all) ...[
        for (final r in rooms) (r.isEmpty ? 'Other' : r, devices.where((d) => d.room == r).toList()),
      ] else if (_room == _favourites)
        (null, devices.where((d) => d.favourite).toList())
      else
        (null, devices.where((d) => d.room == _room).toList()),
    ];

    return Scaffold(
      body: RefreshIndicator(
        onRefresh: () async => hub.reconnectNow(),
        child: CustomScrollView(
          slivers: [
            SliverSafeArea(
              bottom: false,
              sliver: SliverToBoxAdapter(
                child: Padding(
                  padding: EdgeInsets.fromLTRB(sp.xl, sp.l, sp.s, 0),
                  child: Row(crossAxisAlignment: CrossAxisAlignment.end, children: [
                    Expanded(
                      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                        Text(_greeting(), style: theme.textTheme.labelMedium),
                        Text('Home', style: theme.textTheme.headlineMedium),
                      ]),
                    ),
                    IconButton.filledTonal(
                      tooltip: 'Add device',
                      icon: const Icon(Icons.add),
                      onPressed: hub.status == HubStatus.connected
                          ? () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => AddDeviceScreen(hub: hub)))
                          : null,
                    ),
                    _HubMenu(hub: hub),
                  ]),
                ),
              ),
            ),
            SliverToBoxAdapter(child: _Glance(hub: hub, devices: devices, padding: pad)),
            if (hub.scenes.isNotEmpty) ...[
              SliverToBoxAdapter(child: _SectionTitle('Scenes', padding: pad)),
              SliverToBoxAdapter(child: _SceneStrip(hub: hub, padding: pad)),
            ],
            if (devices.isNotEmpty)
              SliverToBoxAdapter(
                child: SizedBox(
                  height: 48,
                  child: ListView(
                    scrollDirection: Axis.horizontal,
                    padding: EdgeInsets.fromLTRB(sp.xl, sp.l, sp.xl, 0),
                    children: [
                      for (final (key, label) in [
                        (_all, 'All'),
                        if (hasFavourites) (_favourites, 'Favourites'),
                        for (final r in rooms) (r, r.isEmpty ? 'Other' : r),
                      ])
                        Padding(
                          padding: EdgeInsets.only(right: sp.s),
                          child: ChoiceChip(
                            label: Text(label),
                            selected: _room == key,
                            onSelected: (_) => setState(() => _room = key),
                          ),
                        ),
                    ],
                  ),
                ),
              ),
            if (devices.isEmpty && hub.status == HubStatus.connected)
              SliverFillRemaining(
                hasScrollBody: false,
                child: _EmptyHome(onAdd: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => AddDeviceScreen(hub: hub)))),
              ),
            for (final (title, list) in sections)
              if (list.isNotEmpty) ...[
                if (title != null) SliverToBoxAdapter(child: _SectionTitle(title, padding: pad)) else SliverToBoxAdapter(child: SizedBox(height: sp.l)),
                SliverPadding(
                  padding: pad,
                  sliver: SliverGrid(
                    gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
                      maxCrossAxisExtent: 200,
                      mainAxisSpacing: 12,
                      crossAxisSpacing: 12,
                      childAspectRatio: 1.08,
                    ),
                    delegate: SliverChildBuilderDelegate(
                      (context, i) => DeviceTile(key: ValueKey(list[i].id), device: list[i], hub: hub),
                      childCount: list.length,
                    ),
                  ),
                ),
              ],
            SliverToBoxAdapter(child: SizedBox(height: sp.xl * 2)),
          ],
        ),
      ),
    );
  }

  static String _greeting() {
    final h = DateTime.now().hour;
    if (h < 5) return 'GOOD NIGHT';
    if (h < 12) return 'GOOD MORNING';
    if (h < 18) return 'GOOD AFTERNOON';
    return 'GOOD EVENING';
  }
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text, {required this.padding});
  final String text;
  final EdgeInsets padding;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Padding(
      padding: padding.copyWith(top: sp.xl + sp.s, bottom: sp.m),
      child: Text(text, style: Theme.of(context).textTheme.titleMedium),
    );
  }
}

/// The house at a glance (Apple Home's status row): what's on, how warm,
/// what needs attention.
class _Glance extends StatelessWidget {
  const _Glance({required this.hub, required this.devices, required this.padding});
  final HubStore hub;
  final List<Device> devices;
  final EdgeInsets padding;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final lightsOn = devices.where((d) => (d.has('dimmer') || d.has('color')) && d.isOn == true).length;
    double? temperature;
    for (final d in devices) {
      final t = (d.cap('sensor')?['readings'] as Map?)?['temperature'];
      if (t is Map && t['value'] is num) temperature ??= (t['value'] as num).toDouble();
      final c = d.cap('climate')?['current'];
      if (c is num) temperature ??= c.toDouble();
    }
    final unlocked = devices.where((d) => d.cap('lock')?['state'] == 'unlocked' || d.cap('lock')?['state'] == 'jammed').toList();
    final locks = devices.where((d) => d.has('lock')).length;
    final offline = devices.where((d) => d.online == 'offline' || d.online == 'unauthorized').length;
    final power = devices.fold<double>(0, (sum, d) => sum + ((d.cap('energy')?['power_w'] as num?)?.toDouble() ?? 0));

    final chips = <Widget>[
      if (temperature != null) _GlanceChip(icon: Icons.thermostat, label: '${temperature.toStringAsFixed(1)}°', sub: 'Inside'),
      _GlanceChip(icon: Icons.lightbulb, label: lightsOn == 0 ? 'Off' : '$lightsOn on', sub: 'Lights', color: lightsOn > 0 ? const Color(0xFFFBBF24) : null),
      if (locks > 0)
        unlocked.isEmpty
            ? const _GlanceChip(icon: Icons.lock, label: 'Locked', sub: 'Doors')
            : _GlanceChip(icon: Icons.lock_open, label: unlocked.length == 1 ? unlocked.first.name : '${unlocked.length} open', sub: 'Unlocked', color: scheme.error),
      if (power > 0) _GlanceChip(icon: Icons.bolt, label: power >= 1000 ? '${(power / 1000).toStringAsFixed(1)} kW' : '${power.round()} W', sub: 'Power'),
      if (offline > 0) _GlanceChip(icon: Icons.cloud_off, label: '$offline', sub: 'Not answering', color: scheme.error),
    ];
    return SizedBox(
      height: 76,
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: padding.copyWith(top: Spacing.of(context).l),
        children: [for (final c in chips) Padding(padding: const EdgeInsets.only(right: 10), child: c)],
      ),
    );
  }
}

class _GlanceChip extends StatelessWidget {
  const _GlanceChip({required this.icon, required this.label, required this.sub, this.color});
  final IconData icon;
  final String label, sub;
  final Color? color;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final tint = color ?? theme.colorScheme.primary;
    return Container(
      padding: const EdgeInsets.fromLTRB(10, 8, 16, 8),
      decoration: BoxDecoration(color: theme.colorScheme.surface, borderRadius: BorderRadius.circular(Shapes.of(context).card)),
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Container(
          width: 32,
          height: 32,
          decoration: BoxDecoration(color: tint.withValues(alpha: 0.16), shape: BoxShape.circle),
          child: Icon(icon, size: 18, color: tint),
        ),
        const SizedBox(width: 10),
        Column(mainAxisAlignment: MainAxisAlignment.center, crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(label, style: theme.textTheme.labelLarge),
          Text(sub, style: theme.textTheme.bodySmall?.copyWith(fontSize: 12)),
        ]),
      ]),
    );
  }
}

/// Scenes as one-tap pills (Home Assistant's Mushroom chips): runs, then a
/// short check mark.
class _SceneStrip extends StatelessWidget {
  const _SceneStrip({required this.hub, required this.padding});
  final HubStore hub;
  final EdgeInsets padding;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 56,
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: padding,
        children: [
          for (final s in hub.scenes)
            Padding(padding: const EdgeInsets.only(right: 10), child: _ScenePill(hub: hub, scene: s)),
        ],
      ),
    );
  }
}

IconData sceneIcon(String name) {
  final n = name.toLowerCase();
  if (n.contains('movie') || n.contains('tv') || n.contains('film')) return Icons.movie_outlined;
  if (n.contains('morning') || n.contains('wake')) return Icons.wb_sunny_outlined;
  if (n.contains('night') || n.contains('bed') || n.contains('sleep')) return Icons.bedtime_outlined;
  if (n.contains('away') || n.contains('leave') || n.contains('out')) return Icons.directions_walk;
  if (n.contains('dinner') || n.contains('cook')) return Icons.restaurant_outlined;
  if (n.contains('read')) return Icons.menu_book_outlined;
  if (n.contains('party')) return Icons.celebration_outlined;
  return Icons.auto_awesome_outlined;
}

class _ScenePill extends StatefulWidget {
  const _ScenePill({required this.hub, required this.scene});
  final HubStore hub;
  final Map<String, dynamic> scene;

  @override
  State<_ScenePill> createState() => _ScenePillState();
}

class _ScenePillState extends State<_ScenePill> {
  bool _running = false;
  bool _done = false;

  Future<void> _run() async {
    if (_running) return;
    HapticFeedback.lightImpact();
    setState(() => _running = true);
    var ok = false;
    await runRequest(context, () async {
      await widget.hub.runScene(widget.scene['id'] as String);
      ok = true;
    });
    if (!mounted) return;
    setState(() {
      _running = false;
      _done = ok;
    });
    if (ok) {
      await Future<void>.delayed(const Duration(milliseconds: 1400));
      if (mounted) setState(() => _done = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final active = _running || _done;
    return Material(
      color: active ? scheme.primary : scheme.surface,
      shape: const StadiumBorder(),
      child: InkWell(
        customBorder: const StadiumBorder(),
        onTap: _run,
        child: AnimatedPadding(
          duration: const Duration(milliseconds: 200),
          padding: const EdgeInsets.fromLTRB(8, 8, 18, 8),
          child: Row(mainAxisSize: MainAxisSize.min, children: [
            AnimatedContainer(
              duration: const Duration(milliseconds: 200),
              width: 36,
              height: 36,
              decoration: BoxDecoration(
                color: active ? scheme.onPrimary.withValues(alpha: 0.16) : scheme.primary.withValues(alpha: 0.14),
                shape: BoxShape.circle,
              ),
              child: _running
                  ? Padding(padding: const EdgeInsets.all(9), child: CircularProgressIndicator(strokeWidth: 2, color: scheme.onPrimary))
                  : Icon(_done ? Icons.check : sceneIcon(widget.scene['name'] as String), size: 20, color: active ? scheme.onPrimary : scheme.primary),
            ),
            const SizedBox(width: 10),
            Text(widget.scene['name'] as String, style: theme.textTheme.labelLarge?.copyWith(color: active ? scheme.onPrimary : scheme.onSurface)),
          ]),
        ),
      ),
    );
  }
}

class _EmptyHome extends StatelessWidget {
  const _EmptyHome({required this.onAdd});
  final VoidCallback onAdd;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.all(32),
      child: Column(mainAxisAlignment: MainAxisAlignment.center, children: [
        Container(
          width: 96,
          height: 96,
          decoration: BoxDecoration(color: theme.colorScheme.primary.withValues(alpha: 0.14), shape: BoxShape.circle),
          child: Icon(Icons.home_outlined, size: 48, color: theme.colorScheme.primary),
        ),
        const SizedBox(height: 20),
        Text('No devices yet', style: theme.textTheme.titleLarge),
        const SizedBox(height: 6),
        Text('Lights, plugs, TVs and more — the hub finds many of them by itself.',
            textAlign: TextAlign.center, style: theme.textTheme.bodySmall),
        const SizedBox(height: 20),
        FilledButton.icon(onPressed: onAdd, icon: const Icon(Icons.add), label: const Text('Add a device')),
      ]),
    );
  }
}

/// Whether a device "is on" for the tile's look: lit, open, heating...
bool isActive(Device d) {
  if (d.isOn != null) return d.isOn!;
  final lock = d.cap('lock')?['state'];
  if (lock != null) return lock == 'unlocked';
  final cover = d.cap('cover');
  if (cover != null) return ((cover['position'] as num?) ?? 0) > 0;
  final climate = d.cap('climate');
  if (climate != null) return climate['mode'] != 'off';
  return false;
}

/// A device as a tile (Apple Home): the icon toggles, the rest opens it.
/// On, it fills with the accent colour; off, it sits back on the surface.
class DeviceTile extends StatefulWidget {
  const DeviceTile({super.key, required this.device, required this.hub});

  final Device device;
  final HubStore hub;

  @override
  State<DeviceTile> createState() => _DeviceTileState();
}

class _DeviceTileState extends State<DeviceTile> {
  bool _busy = false;

  Device get d => widget.device;

  Future<void> _toggle() async {
    if (_busy || !d.reachable) return;
    HapticFeedback.selectionClick();
    setState(() => _busy = true);
    await runRequest(context, () => widget.hub.command(d.id, 'switch', {'on': !(d.isOn ?? false)}));
    if (mounted) setState(() => _busy = false);
  }

  void _open() => Navigator.of(context).push(MaterialPageRoute(builder: (_) => DeviceScreen(hub: widget.hub, id: d.id)));

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final shapes = Shapes.of(context);
    final active = isActive(d) && d.reachable;
    final warn = d.cap('lock')?['state'] == 'unlocked' || d.cap('lock')?['state'] == 'jammed';
    final bg = !active ? scheme.surface : (warn ? scheme.error : scheme.primary);
    final fg = active ? scheme.onPrimary : scheme.onSurface;
    final fgMuted = active ? scheme.onPrimary.withValues(alpha: 0.75) : scheme.onSurfaceVariant;
    final canToggle = d.isOn != null;
    final badge = _badge(d);

    return AnimatedOpacity(
      duration: const Duration(milliseconds: 200),
      opacity: d.reachable ? 1 : 0.5,
      child: AnimatedContainer(
        duration: const Duration(milliseconds: 260),
        curve: Curves.easeOutCubic,
        decoration: BoxDecoration(
          color: bg,
          borderRadius: BorderRadius.circular(shapes.tile),
          boxShadow: active ? [BoxShadow(color: bg.withValues(alpha: 0.35), blurRadius: 18, offset: const Offset(0, 6))] : null,
        ),
        child: Material(
          type: MaterialType.transparency,
          child: InkWell(
            borderRadius: BorderRadius.circular(shapes.tile),
            onTap: _open,
            onLongPress: _open,
            child: Padding(
              padding: const EdgeInsets.all(14),
              child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
                  // The icon is the switch, as in Apple Home.
                  Semantics(
                    button: canToggle,
                    label: canToggle ? (d.isOn! ? 'Switch off' : 'Switch on') : null,
                    child: GestureDetector(
                      onTap: canToggle ? _toggle : _open,
                      child: AnimatedContainer(
                        duration: const Duration(milliseconds: 260),
                        width: 44,
                        height: 44,
                        decoration: BoxDecoration(
                          color: active ? scheme.onPrimary.withValues(alpha: 0.18) : scheme.surfaceContainerHighest,
                          shape: BoxShape.circle,
                        ),
                        child: _busy
                            ? Padding(padding: const EdgeInsets.all(12), child: CircularProgressIndicator(strokeWidth: 2, color: fg))
                            : Icon(iconFor(d, filled: active), size: 24, color: active ? fg : scheme.onSurfaceVariant),
                      ),
                    ),
                  ),
                  const Spacer(),
                  if (badge != null)
                    Text(badge, style: theme.textTheme.labelLarge?.copyWith(color: fg, fontWeight: FontWeight.w700)),
                  if (d.favourite && badge == null) Icon(Icons.star_rounded, size: 18, color: fgMuted),
                ]),
                const Spacer(),
                Text(d.name, maxLines: 2, overflow: TextOverflow.ellipsis, style: theme.textTheme.labelLarge?.copyWith(color: fg, height: 1.15)),
                const SizedBox(height: 2),
                Text(_line(d), maxLines: 1, overflow: TextOverflow.ellipsis, style: theme.textTheme.bodySmall?.copyWith(color: fgMuted, fontSize: 13)),
              ]),
            ),
          ),
        ),
      ),
    );
  }

  /// The big number in the corner, when the device has one.
  static String? _badge(Device d) {
    if (!d.reachable) return null;
    final climate = d.cap('climate');
    if (climate != null) return '${_n(climate['target'])}°';
    final temp = (d.cap('sensor')?['readings'] as Map?)?['temperature'];
    if (temp is Map) return '${_n(temp['value'])}°';
    if (d.isOn == true && d.level != null) return '${d.level}%';
    final cover = d.cap('cover');
    if (cover != null && cover['position'] != null) return '${cover['position']}%';
    return null;
  }

  /// The small line under the name.
  static String _line(Device d) {
    if (d.online == 'offline') return d.lastSeen != null ? summaryOf(d) : 'Not answering';
    if (d.online == 'unauthorized') return 'Pair again';
    final lock = d.cap('lock')?['state'];
    if (lock is String) return lock[0].toUpperCase() + lock.substring(1);
    final climate = d.cap('climate');
    if (climate != null) {
      final mode = '${climate['mode']}';
      final now = climate['current'] is num ? ' · ${_n(climate['current'])}° now' : '';
      return mode[0].toUpperCase() + mode.substring(1) + now;
    }
    final cover = d.cap('cover');
    if (cover != null) {
      final p = (cover['position'] as num?) ?? 0;
      return cover['moving'] == 'opening' ? 'Opening…' : cover['moving'] == 'closing' ? 'Closing…' : p == 0 ? 'Closed' : 'Open';
    }
    final readings = (d.cap('sensor')?['readings'] as Map?)?.cast<String, dynamic>();
    if (readings != null && d.isOn == null) {
      final other = readings.entries.where((e) => e.key != 'temperature').map((e) => '${_n((e.value as Map)['value'])} ${(e.value as Map)['unit'] ?? ''}'.trim());
      return other.isEmpty ? 'Sensor' : other.join(' · ');
    }
    final app = d.cap('media')?['app'];
    if (d.isOn == true && app is Map) return app['label'] as String? ?? 'On';
    final power = d.cap('energy')?['power_w'];
    if (d.isOn == true && power is num && power > 1) return power >= 1000 ? '${(power / 1000).toStringAsFixed(1)} kW' : '${power.round()} W';
    if (d.isOn != null) return d.isOn! ? 'On' : 'Off';
    return d.room;
  }

  static String _n(Object? v) => v is num ? (v == v.roundToDouble() ? v.toStringAsFixed(0) : v.toStringAsFixed(1)) : '–';
}

IconData iconFor(Device d, {bool filled = false}) {
  if (d.has('lock')) return d.cap('lock')?['state'] == 'unlocked' ? Icons.lock_open_rounded : Icons.lock_rounded;
  if (d.has('climate')) return Icons.thermostat_rounded;
  if (d.has('cover')) return filled ? Icons.blinds_rounded : Icons.blinds_closed_rounded;
  if (d.has('media')) return Icons.tv_rounded;
  if (d.has('color') || d.has('dimmer')) return filled ? Icons.lightbulb_rounded : Icons.lightbulb_outline_rounded;
  if (d.has('remote')) return Icons.settings_remote_rounded;
  if (d.has('energy')) return Icons.power_rounded;
  if (d.has('sensor')) return Icons.sensors_rounded;
  if (d.has('switch')) return Icons.power_settings_new_rounded;
  return Icons.devices_other_rounded;
}

/// The one line under the name: why it's greyed out, or its main state.
String summaryOf(Device d) {
  switch (d.online) {
    case 'offline':
      if (d.lastSeen != null) return 'Seen ${_ago(d.lastSeen!)}';
      return 'Not answering';
    case 'unauthorized':
      return 'Needs pairing again (on the hub)';
  }
  final parts = <String>[];
  if (d.isOn != null) parts.add(d.isOn! ? 'On' : 'Off');
  if (d.level != null && d.isOn != false) parts.add('${d.level} %');
  final lock = d.cap('lock')?['state'];
  if (lock is String) parts.add(lock[0].toUpperCase() + lock.substring(1));
  final climate = d.cap('climate');
  if (climate != null) parts.add('${climate['mode']} · ${climate['target']} °C');
  final cover = d.cap('cover');
  if (cover != null && cover['position'] != null) parts.add('${cover['position']} % open');
  final power = d.cap('energy')?['power_w'];
  if (power is num) parts.add('${power.toStringAsFixed(power.abs() < 10 ? 1 : 0)} W');
  final readings = (d.cap('sensor')?['readings'] as Map?)?.values.take(2);
  if (readings != null) {
    for (final r in readings) {
      if (r is Map) parts.add('${r['value']} ${r['unit'] ?? ''}'.trim());
    }
  }
  if (parts.isEmpty && d.room.isNotEmpty) parts.add(d.room);
  return parts.join(' · ');
}

String _ago(DateTime t) {
  final s = DateTime.now().difference(t).inSeconds;
  if (s < 60) return 'just now';
  if (s < 3600) return '${s ~/ 60} min ago';
  if (s < 86400) return '${s ~/ 3600} h ago';
  return '${s ~/ 86400} days ago';
}

/// Why the app isn't connected right now, and what to do -- shown under
/// every tab (main.dart's shell).
class StatusBanner extends StatelessWidget {
  const StatusBanner({super.key, required this.hub});
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final sp = Spacing.of(context);
    final (text, action, actionLabel, isError) = switch (hub.status) {
      HubStatus.connected => (null, null, null, false),
      HubStatus.connecting => ('Connecting to the hub…', null, null, false),
      HubStatus.offline => ('Can\'t reach the hub at ${hub.credentials?.host}. Retrying…', hub.reconnectNow, 'Retry now', false),
      HubStatus.certificateMismatch => (hub.lastError, hub.unpair, 'Pair again', true),
      HubStatus.revoked => ('The hub doesn\'t know this phone any more (removed on the hub).', hub.unpair, 'Pair again', true),
      _ => (null, null, null, false),
    };
    if (text == null) return const SizedBox.shrink();
    return Material(
      color: isError ? scheme.error.withValues(alpha: 0.15) : scheme.surfaceContainerHighest,
      child: Padding(
        padding: EdgeInsets.symmetric(horizontal: sp.l, vertical: sp.s),
        child: Row(children: [
          if (hub.status == HubStatus.connecting)
            Padding(
              padding: EdgeInsets.only(right: sp.m),
              child: const SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2)),
            ),
          Expanded(child: Text(text, style: TextStyle(color: isError ? scheme.error : scheme.onSurface))),
          if (action != null) TextButton(onPressed: action, child: Text(actionLabel!)),
        ]),
      ),
    );
  }
}

class _HubMenu extends StatelessWidget {
  const _HubMenu({required this.hub});
  final HubStore hub;

  Future<void> _changeAddress(BuildContext context) async {
    final c = hub.credentials;
    if (c == null) return;
    final controller = TextEditingController(text: c.port == lanPort ? c.host : '${c.host}:${c.port}');
    final text = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Hub address'),
        content: TextField(
          controller: controller,
          autofocus: true,
          keyboardType: TextInputType.url,
          decoration: const InputDecoration(helperText: 'Same hub, new address. Its certificate is still checked.'),
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, controller.text), child: const Text('Save')),
        ],
      ),
    );
    controller.dispose();
    if (text == null || text.trim().isEmpty) return;
    final (host, port) = splitHostPort(text);
    await hub.changeAddress(host, port);
  }

  Future<void> _unpair(BuildContext context) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Forget this hub?'),
        content: const Text('To use it again, pair again. To also remove this phone from the hub, '
            'use Settings → Paired devices on the hub.'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Forget')),
        ],
      ),
    );
    if (ok == true) await hub.unpair();
  }

  @override
  Widget build(BuildContext context) {
    return PopupMenuButton<String>(
      onSelected: (v) => switch (v) {
        'address' => _changeAddress(context),
        'settings' => Navigator.of(context).push(MaterialPageRoute(builder: (_) => HubSettingsScreen(hub: hub))),
        'unpair' => _unpair(context),
        _ => null,
      },
      itemBuilder: (context) => [
        PopupMenuItem(enabled: false, child: Text('Hub: ${hub.credentials?.host ?? '-'}')),
        if (hub.status == HubStatus.connected) const PopupMenuItem(value: 'settings', child: Text('Hub settings…')),
        const PopupMenuItem(value: 'address', child: Text('Change hub address…')),
        const PopupMenuItem(value: 'unpair', child: Text('Forget this hub…')),
      ],
    );
  }
}
