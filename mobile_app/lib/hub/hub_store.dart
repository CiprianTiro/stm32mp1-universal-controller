import 'dart:async';
import 'dart:math';

import 'package:flutter/foundation.dart';

import 'credentials.dart';
import 'device.dart';
import 'hub_address.dart';
import 'hub_connection.dart';
import 'hub_discovery.dart';

enum HubStatus {
  /// Reading the keystore.
  starting,

  /// No hub paired: show the pairing screen.
  unpaired,
  connecting,
  connected,

  /// Not reachable right now; retrying by itself.
  offline,

  /// The hub at the saved address presented another certificate. Never
  /// retried by itself: the person decides (pair again, or not).
  certificateMismatch,

  /// The hub refused our key (revoked on its screen): pair again.
  revoked,
}

/// The app's view of the hub: the connection, the devices (kept current by
/// the hub's events), and the hub's chosen look. Screens listen to it.
///
/// Once connected: list_devices, get_settings, subscribe -- after that the
/// hub pushes every change, nothing is polled. A lost connection is
/// re-opened with a growing pause (1 s .. 30 s), and the device list is
/// asked for again, since events may have been missed meanwhile.
class HubStore extends ChangeNotifier {
  HubStore({CredentialStore? store}) : _store = store ?? CredentialStore();

  final CredentialStore _store;

  HubStatus status = HubStatus.starting;
  String? lastError;
  HubCredentials? credentials;

  final Map<String, Device> _devices = {};
  Map<String, dynamic> settings = const {'mode': 'dark', 'accent': 'sky', 'density': 'comfortable'};

  /// "Found on your network" (#40): devices the hub sees but hasn't added.
  List<Map<String, dynamic>> found = const [];

  /// Scenes and automations (#47), as the hub keeps them.
  List<Map<String, dynamic>> scenes = const [];
  List<Map<String, dynamic>> automations = const [];

  /// What ran (newest first); null until a screen asked for it.
  List<Map<String, dynamic>>? log;

  /// The device types the hub can add (list_templates), loaded on demand.
  List<Map<String, dynamic>>? templates;

  Future<void> loadTemplates() async {
    if (templates != null) return;
    final reply = await request({'action': 'list_templates'});
    templates = (reply['templates'] as List).cast<Map<String, dynamic>>();
    notifyListeners();
  }

  Map<String, dynamic>? templateOf(Device d) {
    for (final t in templates ?? const <Map<String, dynamic>>[]) {
      if (t['id'] == d.template) return t;
    }
    return null;
  }

  HubConnection? _conn;
  StreamSubscription<Map<String, dynamic>>? _eventSub;
  Timer? _retry;
  int _attempt = 0;
  bool _disposed = false;

  List<Device> get devices => _devices.values.toList();
  Device? device(String id) => _devices[id];

  /// Rooms in the order the devices are listed, "" (no room) last.
  List<String> get rooms {
    final r = _devices.values.map((d) => d.room).toSet().toList()
      ..sort((a, b) => a.isEmpty ? 1 : (b.isEmpty ? -1 : a.toLowerCase().compareTo(b.toLowerCase())));
    return r;
  }

  Future<void> start() async {
    credentials = await _store.load();
    if (credentials == null) {
      _set(HubStatus.unpaired);
    } else {
      unawaited(_connect());
    }
  }

  // ---- Pairing ----

  /// Pairs from a scanned QR code: the certificate to expect is known
  /// beforehand, so a hub presenting another one is refused at once.
  Future<void> pairWithInvite(PairingInvite invite, String clientName) async {
    final conn = await HubConnection.open(invite.host, invite.port, expectFingerprint: invite.fingerprint);
    await _finishPairing(conn, invite.host, invite.port, invite.code, clientName);
  }

  /// Pairing by typed code, step 1: connect and learn which certificate
  /// the hub presents, so the person can compare it with the hub's screen
  /// BEFORE typing the code.
  Future<HubConnection> openForPairing(String host, int port) => HubConnection.open(host, port);

  /// Step 2: the code, typed after comparing the fingerprint.
  Future<void> pairWithCode(HubConnection conn, String host, int port, String code, String clientName) =>
      _finishPairing(conn, host, port, code, clientName);

  Future<void> _finishPairing(HubConnection conn, String host, int port, String code, String clientName) async {
    try {
      final r = await conn.pair(code, clientName);
      final c = HubCredentials(host: host, port: port, clientId: r.clientId, token: r.token, fingerprint: r.hubFingerprint);
      await _store.save(c);
      credentials = c;
    } catch (_) {
      await conn.close();
      rethrow;
    }
    // Pairing also logged this connection in (ws.rs): carry on with it.
    _attempt = 0;
    _set(HubStatus.connecting);
    await _startSession(conn);
  }

  /// Bluetooth setup (#36) paired this phone: the same key and certificate
  /// as a LAN pairing, at the address the hub was found at afterwards.
  Future<void> adoptCredentials(HubCredentials c) async {
    await _store.save(c);
    credentials = c;
    reconnectNow();
  }

  /// Forgets the hub (the key stays valid on the hub until revoked there).
  Future<void> unpair() async {
    _retry?.cancel();
    await _drop();
    await _store.clear();
    credentials = null;
    _devices.clear();
    found = const [];
    scenes = const [];
    automations = const [];
    log = null;
    templates = null;
    _set(HubStatus.unpaired);
  }

  /// The hub moved (new IP): same key, same certificate, new address.
  Future<void> changeAddress(String host, int port) async {
    final c = credentials;
    if (c == null) return;
    credentials = c.withAddress(host, port);
    await _store.save(credentials!);
    reconnectNow();
  }

  void reconnectNow() {
    _retry?.cancel();
    _attempt = 0;
    unawaited(_drop().then((_) => _connect()));
  }

  // ---- Connection ----

  Future<void> _connect() async {
    final c = credentials;
    if (c == null || _disposed) return;
    _set(HubStatus.connecting);
    try {
      final conn = await HubConnection.open(c.host, c.port, expectFingerprint: c.fingerprint);
      try {
        await conn.authenticate(c.token);
      } on HubRefused catch (e) {
        await conn.close();
        lastError = e.message;
        _set(HubStatus.revoked);
        return;
      }
      await _startSession(conn);
    } on CertificateMismatch catch (e) {
      lastError = e.message;
      _set(HubStatus.certificateMismatch);
    } catch (e) {
      lastError = e.toString();
      // Every second failed attempt: has the hub moved to another address?
      if (_attempt.isOdd && await _refind()) return;
      _scheduleRetry();
    }
  }

  /// Looks for the hub over mDNS at another address. Safe to follow: the
  /// connection still has to present the pinned certificate, so another
  /// device announcing itself as a hub gets nothing.
  Future<bool> _refind() async {
    final c = credentials;
    if (c == null) return false;
    for (final hub in await findHubs()) {
      if (hub.host == c.host && hub.port == c.port) continue;
      try {
        final conn = await HubConnection.open(hub.host, hub.port, expectFingerprint: c.fingerprint);
        try {
          await conn.authenticate(c.token);
        } catch (_) {
          await conn.close();
          continue;
        }
        credentials = c.withAddress(hub.host, hub.port);
        await _store.save(credentials!);
        await _startSession(conn);
        return true;
      } catch (_) {
        // Not our hub (other certificate), or not reachable: next.
      }
    }
    return false;
  }

  Future<void> _startSession(HubConnection conn) async {
    _conn = conn;
    _eventSub = conn.events.listen(_onEvent);
    unawaited(conn.done.then((_) {
      if (identical(_conn, conn)) {
        _conn = null;
        _scheduleRetry();
      }
    }));
    try {
      await _reloadDevices();
      final s = await conn.request({'action': 'get_settings'});
      settings = Map<String, dynamic>.from(s)..remove('type');
      await conn.request({'action': 'subscribe'});
      await _reloadFound();
      await _reloadAutomations();
      _attempt = 0;
      lastError = null;
      _set(HubStatus.connected);
    } catch (e) {
      lastError = e.toString();
      await conn.close(); // done -> retry
    }
  }

  Future<void> _reloadDevices() async {
    final reply = await _conn!.request({'action': 'list_devices'});
    _devices
      ..clear()
      ..addEntries((reply['devices'] as List).map((j) {
        final d = Device.fromJson(j as Map<String, dynamic>);
        return MapEntry(d.id, d);
      }));
    notifyListeners();
  }

  Future<void> _reloadFound() async {
    final reply = await _conn!.request({'action': 'list_found'});
    found = (reply['devices'] as List).cast<Map<String, dynamic>>();
    notifyListeners();
  }

  Future<void> _reloadAutomations() async {
    final reply = await _conn!.request({'action': 'list_automations'});
    _takeBook(reply);
  }

  void _takeBook(Map<String, dynamic> reply) {
    scenes = ((reply['scenes'] as List?) ?? const []).cast<Map<String, dynamic>>();
    automations = ((reply['automations'] as List?) ?? const []).cast<Map<String, dynamic>>();
    notifyListeners();
  }

  void _onEvent(Map<String, dynamic> e) {
    switch (e['type']) {
      case 'device_changed':
        final d = Device.fromJson(e['device'] as Map<String, dynamic>);
        _devices[d.id] = d;
        notifyListeners();
      case 'device_removed':
        _devices.remove(e['id']);
        notifyListeners();
      case 'events_lost':
        unawaited(_reloadDevices().catchError((_) {}));
      case 'settings_changed':
        settings = Map<String, dynamic>.from(e)..remove('type');
        notifyListeners();
      case 'found_changed':
        unawaited(_reloadFound().catchError((_) {}));
      case 'automations_changed':
        unawaited(_reloadAutomations().catchError((_) {}));
      case 'automation_ran':
        if (log != null) {
          log = [e['entry'] as Map<String, dynamic>, ...log!];
          notifyListeners();
        }
    }
  }

  void _scheduleRetry() {
    if (_disposed || credentials == null) return;
    _retry?.cancel();
    final seconds = min(30, 1 << min(_attempt, 5));
    _attempt++;
    _set(HubStatus.offline);
    _retry = Timer(Duration(seconds: seconds), _connect);
  }

  Future<void> _drop() async {
    final conn = _conn;
    _conn = null;
    await _eventSub?.cancel();
    _eventSub = null;
    await conn?.close();
  }

  // ---- Requests ----

  /// Sets a capability's state ("command"); the hub answers with the
  /// device as the hardware confirmed it.
  Future<void> command(String id, String capability, Map<String, dynamic> value) async {
    final reply = await _request({'action': 'command', 'id': id, 'capability': capability, 'value': value});
    _takeDevice(reply);
  }

  /// A one-off action (#44): a remote's button, a cover's open/stop...
  Future<dynamic> action(String id, String capability, String name, [Map<String, dynamic>? args]) async {
    final reply = await _request(
        {'action': 'device_action', 'id': id, 'capability': capability, 'name': name, 'args': args ?? {}});
    return reply['result'];
  }

  Future<void> updateInfo(String id, {String? name, String? room, bool? favourite}) async {
    final reply = await _request({
      'action': 'update_device_info',
      'id': id,
      'name': ?name,
      'room': ?room,
      'favourite': ?favourite,
    });
    _takeDevice(reply);
  }

  Future<void> removeDevice(String id) => request({'action': 'remove_device', 'id': id});

  /// A device's secret webhook address (#72).
  Future<String> webhookUrl(String id) async => (await request({'action': 'webhook_url', 'id': id}))['url'] as String;

  Future<void> setSettings(Map<String, dynamic> values) async {
    final reply = await request({'action': 'set_settings', ...values});
    settings = Map<String, dynamic>.from(reply)..remove('type');
    notifyListeners();
  }

  // ---- Scenes and automations (#47) ----

  Future<void> runScene(String id) => request({'action': 'run_scene', 'id': id}, timeout: const Duration(seconds: 90));
  Future<void> runAutomation(String id) =>
      request({'action': 'run_automation', 'id': id}, timeout: const Duration(seconds: 90));

  Future<void> saveScene(Map<String, dynamic> scene) async => _takeBook(await request({'action': 'save_scene', 'scene': scene}));
  Future<void> deleteScene(String id) async => _takeBook(await request({'action': 'delete_scene', 'id': id}));
  Future<void> captureScene(String name, List<String> devices) async =>
      _takeBook(await request({'action': 'capture_scene', 'name': name, 'devices': devices}));

  Future<void> saveAutomation(Map<String, dynamic> a) async =>
      _takeBook(await request({'action': 'save_automation', 'automation': a}));
  Future<void> deleteAutomation(String id) async => _takeBook(await request({'action': 'delete_automation', 'id': id}));
  Future<void> setAutomationEnabled(String id, bool enabled) async =>
      _takeBook(await request({'action': 'set_automation_enabled', 'id': id, 'enabled': enabled}));

  Future<void> loadLog() async {
    final reply = await request({'action': 'get_automation_log'});
    // The hub lists oldest first; the screen shows the newest on top.
    log = ((reply['entries'] as List).cast<Map<String, dynamic>>()).reversed.toList();
    notifyListeners();
  }

  /// Any request (the wizard's, the discovery screens'). Replies of type
  /// "error" are thrown as [HubRefused].
  Future<Map<String, dynamic>> request(Map<String, dynamic> message, {Duration? timeout}) {
    final demo = _demo;
    if (demo != null) return demo(message);
    final conn = _conn;
    if (conn == null || status != HubStatus.connected) {
      return Future.error(const HubException('not connected to the hub'));
    }
    return conn.request(message, timeout: timeout);
  }

  Future<Map<String, dynamic>> _request(Map<String, dynamic> message) => request(message);

  void _takeDevice(Map<String, dynamic> reply) {
    final j = reply['device'];
    if (j is Map<String, dynamic>) {
      final d = Device.fromJson(j);
      _devices[d.id] = d;
      notifyListeners();
    }
  }

  // ---- Demo mode (--dart-define=DEMO=true): a fake hub in the app, for
  // trying the screens and taking screenshots without a DK2. ----

  Future<Map<String, dynamic>> Function(Map<String, dynamic>)? _demo;

  /// Runs on [handler] instead of a hub: it answers every request like
  /// ws.rs would. Events are fed with [demoEvent].
  Future<void> startDemo(Future<Map<String, dynamic>> Function(Map<String, dynamic>) handler) async {
    _demo = handler;
    credentials = HubCredentials(host: 'demo', port: lanPort, clientId: 'demo', token: '', fingerprint: '');
    status = HubStatus.connected;
    await _reloadDemo();
  }

  Future<void> _reloadDemo() async {
    final devices = await request({'action': 'list_devices'});
    _devices
      ..clear()
      ..addEntries((devices['devices'] as List).map((j) {
        final d = Device.fromJson(j as Map<String, dynamic>);
        return MapEntry(d.id, d);
      }));
    settings = Map<String, dynamic>.from(await request({'action': 'get_settings'}))..remove('type');
    found = ((await request({'action': 'list_found'}))['devices'] as List).cast<Map<String, dynamic>>();
    _takeBook(await request({'action': 'list_automations'}));
  }

  void demoEvent(Map<String, dynamic> event) => _onEvent(event);

  void _set(HubStatus s) {
    status = s;
    if (!_disposed) notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    _retry?.cancel();
    unawaited(_drop());
    super.dispose();
  }
}
