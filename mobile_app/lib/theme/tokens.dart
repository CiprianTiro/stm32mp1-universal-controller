import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

/// The hub's design tokens (#39), read from the same tokens.json the
/// touchscreen compiles in (assets/tokens.json is a symlink to
/// linux_a7/ui_layer/ui/tokens.json). A preset = mode + accent + density,
/// chosen on the hub (get_settings / settings_changed); the phone follows,
/// so both look alike.
class DesignTokens {
  DesignTokens(this._json);

  final Map<String, dynamic> _json;

  static Future<DesignTokens> load() async =>
      DesignTokens(jsonDecode(await rootBundle.loadString('assets/tokens.json')) as Map<String, dynamic>);

  Map<String, dynamic> get _defaults => _json['defaults'] as Map<String, dynamic>;

  /// The palette for a mode; "auto" is light between the hours tokens.json
  /// names, dark otherwise (the hub uses its own clock; the phone, being in
  /// the same house, uses its own).
  String resolveMode(String mode, DateTime now) {
    if (mode != 'auto') return (_json['modes'] as Map).containsKey(mode) ? mode : _defaults['mode'] as String;
    final auto = _json['auto'] as Map<String, dynamic>;
    final light = now.hour >= (auto['light-from-hour'] as int) && now.hour < (auto['dark-from-hour'] as int);
    return light ? 'light' : 'dark';
  }

  ThemeData theme({required String mode, required String accent, required String density, DateTime? now}) {
    final resolved = resolveMode(mode, now ?? DateTime.now());
    final dark = resolved == 'dark';
    final palette = (_json['modes'] as Map<String, dynamic>)[resolved] as Map<String, dynamic>;
    final accents = (_json['accents'] as List).cast<Map<String, dynamic>>();
    final a = accents.firstWhere((x) => x['id'] == accent,
        orElse: () => accents.firstWhere((x) => x['id'] == _defaults['accent']));
    final densities = _json['density'] as Map<String, dynamic>;
    final d = (densities[density] ?? densities[_defaults['density']]) as Map<String, dynamic>;
    final type = _json['typography'] as Map<String, dynamic>;
    final radii = _json['radii'] as Map<String, dynamic>;

    Color c(Map<String, dynamic> m, String key) => _color(m[key] as String);
    final accentColor = _color(a[dark ? 'dark' : 'light'] as String);
    final onAccent = _color(a[dark ? 'on-accent-dark' : 'on-accent-light'] as String);
    final touch = (d['touch-height'] as num).toDouble();

    final scheme = ColorScheme(
      brightness: dark ? Brightness.dark : Brightness.light,
      primary: accentColor,
      onPrimary: onAccent,
      secondary: accentColor,
      onSecondary: onAccent,
      error: c(palette, 'error'),
      onError: onAccent,
      surface: c(palette, 'surface'),
      onSurface: c(palette, 'text'),
      onSurfaceVariant: c(palette, 'text-muted'),
      surfaceContainerHighest: c(palette, 'surface-pressed'),
      outline: c(palette, 'outline'),
      outlineVariant: c(palette, 'outline'),
    );
    double t(String k) => (type[k] as num).toDouble();
    final text = c(palette, 'text');

    // Shapes: the hub's radii are sized for a 7" touchscreen's dense
    // lists; a phone's tiles and sheets read better rounder (the look of
    // Apple Home / Material 3). Derived from the token so a change to it
    // still carries over.
    final medium = (radii['medium'] as num).toDouble();
    final shapes = Shapes(card: medium * 2.5, tile: medium * 2.75, control: medium * 1.5);
    final cardRadius = BorderRadius.circular(shapes.card);
    final controlRadius = BorderRadius.circular(shapes.control);
    final background = c(palette, 'background');
    final surface = c(palette, 'surface');
    final muted = c(palette, 'text-muted');

    return ThemeData(
      useMaterial3: true,
      colorScheme: scheme,
      scaffoldBackgroundColor: background,
      visualDensity: density == 'compact' ? VisualDensity.compact : VisualDensity.standard,
      splashFactory: InkSparkle.splashFactory,
      textTheme: TextTheme(
        // Big numbers: a thermostat's target, a dial's value.
        displaySmall: TextStyle(fontSize: t('display'), color: text, fontWeight: FontWeight.w300, height: 1.0),
        // Large page titles, as iOS draws them.
        headlineMedium: TextStyle(fontSize: t('title') * 1.4, color: text, fontWeight: FontWeight.w700, letterSpacing: -0.5),
        titleLarge: TextStyle(fontSize: t('title'), color: text, fontWeight: FontWeight.w700, letterSpacing: -0.3),
        titleMedium: TextStyle(fontSize: t('heading'), color: text, fontWeight: FontWeight.w600),
        titleSmall: TextStyle(fontSize: t('subtitle'), color: text, fontWeight: FontWeight.w600),
        bodyLarge: TextStyle(fontSize: t('body'), color: text),
        bodyMedium: TextStyle(fontSize: t('body'), color: text),
        bodySmall: TextStyle(fontSize: t('caption'), color: muted),
        labelLarge: TextStyle(fontSize: t('body'), fontWeight: FontWeight.w600),
        labelMedium: TextStyle(fontSize: t('caption'), color: muted, fontWeight: FontWeight.w600, letterSpacing: 0.4),
      ),
      appBarTheme: AppBarTheme(
        backgroundColor: background,
        surfaceTintColor: Colors.transparent,
        foregroundColor: text,
        elevation: 0,
        scrolledUnderElevation: 0,
        centerTitle: false,
        titleTextStyle: TextStyle(fontSize: t('heading'), color: text, fontWeight: FontWeight.w700),
      ),
      // Tonal surfaces instead of outlined boxes: depth from colour, not lines.
      cardTheme: CardThemeData(
        color: surface,
        elevation: 0,
        margin: EdgeInsets.zero,
        clipBehavior: Clip.antiAlias,
        shape: RoundedRectangleBorder(borderRadius: cardRadius),
      ),
      listTileTheme: ListTileThemeData(shape: RoundedRectangleBorder(borderRadius: cardRadius), iconColor: muted),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(minimumSize: Size(touch, touch), shape: const StadiumBorder()),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(minimumSize: Size(touch, touch), shape: const StadiumBorder(), side: BorderSide(color: c(palette, 'outline'))),
      ),
      textButtonTheme: TextButtonThemeData(style: TextButton.styleFrom(shape: const StadiumBorder())),
      chipTheme: ChipThemeData(
        shape: const StadiumBorder(),
        side: BorderSide.none,
        backgroundColor: surface,
        selectedColor: accentColor.withValues(alpha: dark ? 0.22 : 0.16),
        labelStyle: TextStyle(fontSize: t('caption') + 1, color: text, fontWeight: FontWeight.w600),
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 6),
        showCheckmark: false,
      ),
      segmentedButtonTheme: SegmentedButtonThemeData(
        style: SegmentedButton.styleFrom(
          backgroundColor: surface,
          selectedBackgroundColor: accentColor.withValues(alpha: dark ? 0.25 : 0.18),
          side: BorderSide.none,
          shape: const StadiumBorder(),
        ),
      ),
      sliderTheme: SliderThemeData(
        trackHeight: 6,
        inactiveTrackColor: c(palette, 'surface-pressed'),
        thumbShape: const RoundSliderThumbShape(enabledThumbRadius: 10),
        overlayShape: SliderComponentShape.noOverlay,
      ),
      switchTheme: SwitchThemeData(
        trackOutlineColor: WidgetStateProperty.all(Colors.transparent),
        trackColor: WidgetStateProperty.resolveWith((s) => s.contains(WidgetState.selected) ? accentColor : c(palette, 'surface-pressed')),
        thumbColor: WidgetStateProperty.resolveWith((s) => s.contains(WidgetState.selected) ? onAccent : c(palette, 'knob')),
      ),
      navigationBarTheme: NavigationBarThemeData(
        backgroundColor: background,
        surfaceTintColor: Colors.transparent,
        indicatorColor: accentColor.withValues(alpha: dark ? 0.22 : 0.16),
        labelTextStyle: WidgetStateProperty.all(TextStyle(fontSize: t('caption') - 2, fontWeight: FontWeight.w600, color: text)),
        height: 68,
      ),
      floatingActionButtonTheme: FloatingActionButtonThemeData(
        backgroundColor: accentColor,
        foregroundColor: onAccent,
        elevation: 0,
        shape: const StadiumBorder(),
      ),
      dialogTheme: DialogThemeData(backgroundColor: surface, shape: RoundedRectangleBorder(borderRadius: cardRadius)),
      bottomSheetTheme: BottomSheetThemeData(
        backgroundColor: surface,
        showDragHandle: true,
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.vertical(top: Radius.circular(shapes.card + 8))),
      ),
      snackBarTheme: SnackBarThemeData(behavior: SnackBarBehavior.floating, shape: RoundedRectangleBorder(borderRadius: controlRadius)),
      inputDecorationTheme: InputDecorationTheme(
        filled: true,
        fillColor: surface,
        border: OutlineInputBorder(borderRadius: controlRadius, borderSide: BorderSide.none),
        enabledBorder: OutlineInputBorder(borderRadius: controlRadius, borderSide: BorderSide.none),
        focusedBorder: OutlineInputBorder(borderRadius: controlRadius, borderSide: BorderSide(color: accentColor, width: 2)),
      ),
      pageTransitionsTheme: const PageTransitionsTheme(builders: {
        TargetPlatform.android: FadeForwardsPageTransitionsBuilder(),
        TargetPlatform.iOS: CupertinoPageTransitionsBuilder(),
      }),
      extensions: [Spacing.fromJson(d), shapes],
    );
  }

  static Color _color(String hex) => Color(int.parse('FF${hex.substring(1)}', radix: 16));
}

/// The density's spacing steps, for layouts: `Theme.of(context).extension<Spacing>()!`.
class Spacing extends ThemeExtension<Spacing> {
  const Spacing({required this.xs, required this.s, required this.m, required this.l, required this.xl});

  factory Spacing.fromJson(Map<String, dynamic> d) {
    double v(String k) => (d[k] as num).toDouble();
    return Spacing(xs: v('space-xs'), s: v('space-s'), m: v('space-m'), l: v('space-l'), xl: v('space-xl'));
  }

  final double xs, s, m, l, xl;

  static Spacing of(BuildContext context) =>
      Theme.of(context).extension<Spacing>() ?? const Spacing(xs: 6, s: 8, m: 10, l: 12, xl: 14);

  @override
  Spacing copyWith({double? xs, double? s, double? m, double? l, double? xl}) =>
      Spacing(xs: xs ?? this.xs, s: s ?? this.s, m: m ?? this.m, l: l ?? this.l, xl: xl ?? this.xl);

  @override
  Spacing lerp(Spacing? other, double t) => other ?? this;
}

/// The app's corner radii (see DesignTokens.theme): `Shapes.of(context)`.
class Shapes extends ThemeExtension<Shapes> {
  const Shapes({required this.card, required this.tile, required this.control});

  final double card, tile, control;

  static Shapes of(BuildContext context) => Theme.of(context).extension<Shapes>() ?? const Shapes(card: 20, tile: 22, control: 12);

  @override
  Shapes copyWith({double? card, double? tile, double? control}) =>
      Shapes(card: card ?? this.card, tile: tile ?? this.tile, control: control ?? this.control);

  @override
  Shapes lerp(Shapes? other, double t) => other ?? this;
}
