import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../hub/device.dart';
import '../hub/hub_store.dart';
import 'capability_controls.dart';

/// The big control at the top of a device's page, chosen by what the
/// device is -- the way Apple Home and Nest give each kind of device one
/// obvious control:
///   light (dimmer)      a tall pill slider (Apple Home), power button below
///   climate             a round dial to drag (Nest)
///   cover               a pill slider for the position + open/stop/close
///   lock                one big button (unlocking asks first, #77)
///   anything switchable one big power button
/// Returns the capabilities it took care of, so the sections below skip them.
class DeviceHero {
  DeviceHero._(this.widget, this.handles);

  final Widget? widget;
  final Set<String> handles;

  factory DeviceHero.of(Device d, HubStore hub) {
    if (d.has('climate')) return DeviceHero._(_ClimateHero(device: d, hub: hub), {'climate'});
    if (d.has('dimmer')) return DeviceHero._(_LightHero(device: d, hub: hub), {'dimmer', 'switch'});
    if (d.has('cover')) return DeviceHero._(_CoverHero(device: d, hub: hub), {'cover'});
    if (d.has('lock')) return DeviceHero._(_LockHero(device: d, hub: hub), {'lock'});
    if (d.has('switch')) return DeviceHero._(_PowerHero(device: d, hub: hub), {'switch'});
    return DeviceHero._(null, const {});
  }
}

// ---------------------------------------------------------------------------
// Pill slider
// ---------------------------------------------------------------------------

/// A tall rounded slider filled from the bottom (Apple Home's brightness
/// control). Follows the finger, sends once on release.
class PillSlider extends StatefulWidget {
  const PillSlider({
    super.key,
    required this.value,
    required this.onCommit,
    this.width = 120,
    this.height = 300,
    this.color,
    this.icon,
    this.label,
  });

  /// 0..100.
  final double value;
  final ValueChanged<int> onCommit;
  final double width, height;
  final Color? color;
  final IconData? icon;
  final String Function(int)? label;

  @override
  State<PillSlider> createState() => _PillSliderState();
}

class _PillSliderState extends State<PillSlider> {
  double? _drag;
  int _lastTick = -1;

  void _update(Offset local) {
    final v = ((1 - local.dy / widget.height) * 100).clamp(0, 100).toDouble();
    // A tick every 10 %, as iOS sliders do.
    final tick = v ~/ 10;
    if (tick != _lastTick) {
      HapticFeedback.selectionClick();
      _lastTick = tick;
    }
    setState(() => _drag = v);
  }

  void _end() {
    final v = _drag;
    if (v != null) widget.onCommit(v.round());
    setState(() => _drag = null);
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final value = (_drag ?? widget.value).clamp(0, 100).toDouble();
    final color = widget.color ?? scheme.primary;
    final shown = value.round();
    final onFill = value > 12;
    return Semantics(
      slider: true,
      value: '$shown %',
      increasedValue: '${math.min(100, shown + 10)} %',
      decreasedValue: '${math.max(0, shown - 10)} %',
      onIncrease: () => widget.onCommit(math.min(100, shown + 10)),
      onDecrease: () => widget.onCommit(math.max(0, shown - 10)),
      child: GestureDetector(
        onVerticalDragStart: (d) => _update(d.localPosition),
        onVerticalDragUpdate: (d) => _update(d.localPosition),
        onVerticalDragEnd: (_) => _end(),
        onTapDown: (d) => _update(d.localPosition),
        onTapUp: (_) => _end(),
        child: AnimatedScale(
          scale: _drag != null ? 1.03 : 1,
          duration: const Duration(milliseconds: 160),
          child: Container(
            width: widget.width,
            height: widget.height,
            decoration: BoxDecoration(
              color: scheme.surface,
              borderRadius: BorderRadius.circular(widget.width * 0.32),
              boxShadow: [BoxShadow(color: color.withValues(alpha: value > 0 ? 0.25 : 0), blurRadius: 30, offset: const Offset(0, 10))],
            ),
            clipBehavior: Clip.antiAlias,
            child: Stack(alignment: Alignment.bottomCenter, children: [
              AnimatedContainer(
                duration: Duration(milliseconds: _drag != null ? 0 : 220),
                curve: Curves.easeOutCubic,
                height: widget.height * value / 100,
                decoration: BoxDecoration(
                  gradient: LinearGradient(
                    begin: Alignment.bottomCenter,
                    end: Alignment.topCenter,
                    colors: [color, Color.lerp(color, Colors.white, 0.25)!],
                  ),
                ),
              ),
              Positioned(
                bottom: 22,
                child: Column(children: [
                  Text(widget.label?.call(shown) ?? '$shown%',
                      style: TextStyle(fontSize: 22, fontWeight: FontWeight.w700, color: onFill ? scheme.onPrimary : scheme.onSurface)),
                  if (widget.icon != null) ...[
                    const SizedBox(height: 6),
                    Icon(widget.icon, size: 28, color: onFill ? scheme.onPrimary : scheme.onSurfaceVariant),
                  ],
                ]),
              ),
            ]),
          ),
        ),
      ),
    );
  }
}

/// A big round button (power, lock).
class RoundButton extends StatelessWidget {
  const RoundButton({super.key, required this.icon, required this.active, required this.onTap, this.size = 76, this.color, this.label});
  final IconData icon;
  final bool active;
  final VoidCallback? onTap;
  final double size;
  final Color? color;
  final String? label;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final c = color ?? scheme.primary;
    return Column(mainAxisSize: MainAxisSize.min, children: [
      AnimatedContainer(
        duration: const Duration(milliseconds: 220),
        width: size,
        height: size,
        decoration: BoxDecoration(
          shape: BoxShape.circle,
          color: active ? c : scheme.surface,
          boxShadow: active ? [BoxShadow(color: c.withValues(alpha: 0.4), blurRadius: 24, offset: const Offset(0, 8))] : null,
        ),
        child: Material(
          type: MaterialType.transparency,
          shape: const CircleBorder(),
          child: InkWell(
            customBorder: const CircleBorder(),
            onTap: onTap == null
                ? null
                : () {
                    HapticFeedback.mediumImpact();
                    onTap!();
                  },
            child: Icon(icon, size: size * 0.42, color: active ? scheme.onPrimary : scheme.onSurfaceVariant),
          ),
        ),
      ),
      if (label != null) ...[
        const SizedBox(height: 8),
        Text(label!, style: Theme.of(context).textTheme.labelMedium),
      ],
    ]);
  }
}

// ---------------------------------------------------------------------------
// The heroes
// ---------------------------------------------------------------------------

class _LightHero extends StatelessWidget {
  const _LightHero({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final d = device;
    final on = d.isOn ?? (d.level ?? 0) > 0;
    final color = _lightColor(context, d);
    return Column(children: [
      PillSlider(
        value: on ? (d.level ?? 0).toDouble() : 0,
        color: color,
        icon: on ? Icons.lightbulb_rounded : Icons.lightbulb_outline_rounded,
        label: (v) => v == 0 ? 'Off' : '$v%',
        onCommit: (v) => runRequest(context, () async {
          // 0 is "off" (as in Apple Home); moving up from off switches on.
          if (v == 0) {
            if (d.has('switch')) await hub.command(d.id, 'switch', {'on': false});
            return;
          }
          if (d.has('switch') && d.isOn != true) await hub.command(d.id, 'switch', {'on': true});
          await hub.command(d.id, 'dimmer', {'level': v});
        }),
      ),
      if (d.has('switch')) ...[
        const SizedBox(height: 24),
        RoundButton(
          icon: Icons.power_settings_new_rounded,
          active: on,
          size: 60,
          onTap: () => runRequest(context, () => hub.command(d.id, 'switch', {'on': !on})),
        ),
      ],
    ]);
  }

  /// The slider in the light's own colour (or its white's warmth).
  static Color? _lightColor(BuildContext context, Device d) {
    final c = d.cap('color');
    if (c == null) return null;
    final hex = c['hex'];
    if (hex is String && hex.length == 7) return Color(int.parse('FF${hex.substring(1)}', radix: 16));
    final k = (c['kelvin'] as num?)?.toDouble();
    if (k != null) {
      // 2700 K warm amber .. 6500 K cool white.
      final t = ((k - 2700) / (6500 - 2700)).clamp(0.0, 1.0);
      return Color.lerp(const Color(0xFFFFB45E), const Color(0xFFE8F0FF), t);
    }
    return null;
  }
}

class _CoverHero extends StatelessWidget {
  const _CoverHero({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final c = device.cap('cover')!;
    final pos = (c['position'] as num?)?.toDouble();
    final canPosition = c['can_position'] == true && pos != null;
    void act(String name) => runRequest(context, () => hub.action(device.id, 'cover', name));
    return Column(children: [
      if (canPosition)
        PillSlider(
          value: pos,
          icon: pos > 0 ? Icons.blinds_rounded : Icons.blinds_closed_rounded,
          label: (v) => v == 0 ? 'Closed' : v == 100 ? 'Open' : '$v%',
          onCommit: (v) => runRequest(context, () => hub.command(device.id, 'cover', {'position': v})),
        ),
      const SizedBox(height: 24),
      Row(mainAxisAlignment: MainAxisAlignment.center, children: [
        RoundButton(icon: Icons.keyboard_arrow_down_rounded, active: false, size: 60, label: 'Close', onTap: () => act('close')),
        const SizedBox(width: 24),
        RoundButton(icon: Icons.stop_rounded, active: c['moving'] != 'stopped' && c['moving'] != null, size: 60, label: 'Stop', onTap: () => act('stop')),
        const SizedBox(width: 24),
        RoundButton(icon: Icons.keyboard_arrow_up_rounded, active: false, size: 60, label: 'Open', onTap: () => act('open')),
      ]),
    ]);
  }
}

class _PowerHero extends StatelessWidget {
  const _PowerHero({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final on = device.isOn ?? false;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 24),
      child: RoundButton(
        icon: Icons.power_settings_new_rounded,
        active: on,
        size: 132,
        label: on ? 'ON' : 'OFF',
        onTap: () => runRequest(context, () => hub.command(device.id, 'switch', {'on': !on})),
      ),
    );
  }
}

class _LockHero extends StatelessWidget {
  const _LockHero({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  /// THE UNLOCK RULE (#77): the person confirms; only then "confirmed".
  Future<void> _toggle(BuildContext context, String state) async {
    if (state == 'unlocked') {
      await runRequest(context, () => hub.command(device.id, 'lock', {'state': 'locked'}));
      return;
    }
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        icon: const Icon(Icons.lock_open_rounded),
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
    final scheme = Theme.of(context).colorScheme;
    final state = device.cap('lock')!['state'] as String? ?? 'unknown';
    final (label, icon) = switch (state) {
      'locked' => ('LOCKED · tap to unlock', Icons.lock_rounded),
      'unlocked' => ('UNLOCKED · tap to lock', Icons.lock_open_rounded),
      'jammed' => ('JAMMED — check the door', Icons.warning_amber_rounded),
      _ => ('UNKNOWN', Icons.help_outline_rounded),
    };
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 24),
      child: RoundButton(
        icon: icon,
        active: state != 'locked',
        color: state == 'locked' ? null : scheme.error,
        size: 132,
        label: label,
        onTap: () => _toggle(context, state),
      ),
    );
  }
}

// ---------------------------------------------------------------------------
// Thermostat dial (Nest)
// ---------------------------------------------------------------------------

class _ClimateHero extends StatefulWidget {
  const _ClimateHero({required this.device, required this.hub});
  final Device device;
  final HubStore hub;

  @override
  State<_ClimateHero> createState() => _ClimateHeroState();
}

class _ClimateHeroState extends State<_ClimateHero> {
  double? _drag;
  static const _sweep = 1.5 * math.pi; // 270°
  static const _start = 0.75 * math.pi; // from bottom-left

  Map<String, dynamic> get c => widget.device.cap('climate')!;
  double get _min => (c['min'] as num?)?.toDouble() ?? 16;
  double get _max => (c['max'] as num?)?.toDouble() ?? 30;
  double get _step => (c['step'] as num?)?.toDouble() ?? 0.5;
  double get _target => (c['target'] as num?)?.toDouble() ?? 21;

  double _snap(double v) => ((v / _step).round() * _step).clamp(_min, _max).toDouble();

  void _send({double? target, String? mode, String? fan}) {
    final m = mode ?? c['mode'] as String? ?? 'off';
    final f = fan ?? c['fan'] as String?;
    runRequest(context, () => widget.hub.command(widget.device.id, 'climate', {'mode': m, 'target': target ?? _target, 'fan': ?f}));
  }

  void _fromPosition(Offset p, Size size) {
    final center = size.center(Offset.zero);
    final a = math.atan2(p.dy - center.dy, p.dx - center.dx);
    // Angle from the dial's start, 0.._sweep.
    var rel = (a - _start) % (2 * math.pi);
    if (rel > _sweep) rel = rel > _sweep + (2 * math.pi - _sweep) / 2 ? 0 : _sweep;
    final v = _snap(_min + rel / _sweep * (_max - _min));
    if (v != _drag) HapticFeedback.selectionClick();
    setState(() => _drag = v);
  }

  static String _fmt(double v) => v == v.roundToDouble() ? v.toStringAsFixed(0) : v.toStringAsFixed(1);

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final mode = c['mode'] as String? ?? 'off';
    final current = (c['current'] as num?)?.toDouble();
    final modes = ((c['modes'] as List?) ?? const ['off', 'heat', 'cool', 'auto']).cast<String>();
    final fans = ((c['fans'] as List?) ?? const []).cast<String>();
    final value = _drag ?? _target;
    final color = switch (mode) {
      'heat' => const Color(0xFFFB923C),
      'cool' => const Color(0xFF38BDF8),
      'auto' => const Color(0xFF34D399),
      'dry' => const Color(0xFFA78BFA),
      'fan' => const Color(0xFF94A3B8),
      _ => scheme.onSurfaceVariant,
    };
    const size = Size(280, 280);
    final modeLabel = switch (mode) {
      'heat' => 'Heating',
      'cool' => 'Cooling',
      'auto' => 'Auto',
      'dry' => 'Drying',
      'fan' => 'Fan',
      _ => 'Off',
    };

    return Column(children: [
      GestureDetector(
        onPanStart: (d) => _fromPosition(d.localPosition, size),
        onPanUpdate: (d) => _fromPosition(d.localPosition, size),
        onPanEnd: (_) {
          final v = _drag;
          setState(() => _drag = null);
          if (v != null && v != _target) _send(target: v);
        },
        child: CustomPaint(
          size: size,
          painter: _DialPainter(
            fraction: (value - _min) / (_max - _min),
            currentFraction: current == null ? null : ((current - _min) / (_max - _min)).clamp(0.0, 1.0),
            color: color,
            track: scheme.surface,
            knob: scheme.onSurface,
            start: _start,
            sweep: _sweep,
          ),
          child: SizedBox.fromSize(
            size: size,
            child: Column(mainAxisAlignment: MainAxisAlignment.center, children: [
              Text(modeLabel.toUpperCase(), style: theme.textTheme.labelMedium?.copyWith(color: color)),
              const SizedBox(height: 4),
              Row(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
                Text(_fmt(value), style: theme.textTheme.displaySmall?.copyWith(fontSize: 64)),
                Padding(padding: const EdgeInsets.only(top: 8), child: Text('°C', style: theme.textTheme.titleMedium)),
              ]),
              if (current != null) Text('${_fmt(current)}° in the room', style: theme.textTheme.bodySmall),
            ]),
          ),
        ),
      ),
      Row(mainAxisAlignment: MainAxisAlignment.center, children: [
        RoundButton(icon: Icons.remove_rounded, active: false, size: 52, onTap: _target - _step >= _min ? () => _send(target: _snap(_target - _step)) : null),
        const SizedBox(width: 40),
        RoundButton(icon: Icons.add_rounded, active: false, size: 52, onTap: _target + _step <= _max ? () => _send(target: _snap(_target + _step)) : null),
      ]),
      const SizedBox(height: 20),
      Wrap(alignment: WrapAlignment.center, spacing: 8, runSpacing: 8, children: [
        for (final m in modes)
          ChoiceChip(
            label: Text(m[0].toUpperCase() + m.substring(1)),
            selected: m == mode,
            onSelected: (_) => _send(mode: m),
          ),
      ]),
      if (fans.isNotEmpty) ...[
        const SizedBox(height: 12),
        Wrap(alignment: WrapAlignment.center, spacing: 8, children: [
          Padding(padding: const EdgeInsets.only(top: 10), child: Icon(Icons.air_rounded, size: 18, color: scheme.onSurfaceVariant)),
          for (final f in fans)
            ChoiceChip(label: Text(f), selected: f == c['fan'], onSelected: (_) => _send(fan: f)),
        ]),
      ],
    ]);
  }
}

class _DialPainter extends CustomPainter {
  _DialPainter({
    required this.fraction,
    required this.currentFraction,
    required this.color,
    required this.track,
    required this.knob,
    required this.start,
    required this.sweep,
  });

  final double fraction;
  final double? currentFraction;
  final Color color, track, knob;
  final double start, sweep;

  @override
  void paint(Canvas canvas, Size size) {
    const stroke = 22.0;
    final center = size.center(Offset.zero);
    final radius = size.width / 2 - stroke;
    final rect = Rect.fromCircle(center: center, radius: radius);
    final base = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = stroke
      ..strokeCap = StrokeCap.round;

    canvas.drawArc(rect, start, sweep, false, base..color = track);
    final f = fraction.clamp(0.0, 1.0);
    if (f > 0) {
      final shader = SweepGradient(
        startAngle: 0,
        endAngle: 2 * math.pi,
        colors: [color.withValues(alpha: 0.55), color],
        stops: const [0, 1],
        transform: GradientRotation(start),
      ).createShader(rect);
      canvas.drawArc(rect, start, sweep * f, false, Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = stroke
        ..strokeCap = StrokeCap.round
        ..shader = shader);
    }
    // Where the room is now: a small tick on the track.
    final cf = currentFraction;
    if (cf != null) {
      final a = start + sweep * cf;
      final p1 = center + Offset(math.cos(a), math.sin(a)) * (radius - stroke / 2 - 4);
      final p2 = center + Offset(math.cos(a), math.sin(a)) * (radius - stroke / 2 - 14);
      canvas.drawLine(p1, p2, Paint()
        ..color = knob.withValues(alpha: 0.6)
        ..strokeWidth = 3
        ..strokeCap = StrokeCap.round);
    }
    // The knob.
    final a = start + sweep * f;
    final k = center + Offset(math.cos(a), math.sin(a)) * radius;
    canvas.drawCircle(k, stroke / 2 + 4, Paint()..color = knob);
    canvas.drawCircle(k, stroke / 2 - 3, Paint()..color = color);
  }

  @override
  bool shouldRepaint(_DialPainter old) =>
      old.fraction != fraction || old.currentFraction != currentFraction || old.color != color || old.track != track;
}
