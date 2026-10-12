import 'dart:async';

import 'package:flutter/material.dart';

import '../hub/hub_connection.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import 'device_screen.dart';

/// The add-device wizard (#40), drawn from what the hub sends: the steps
/// and their checks live in the backend (wizard.rs), the same ones the
/// touchscreen shows -- so a new device type needs no app update.
///
/// [start] is the request that opens the session: wizard_start (a type, a
/// way in, or a found device), wizard_reauth ("pair again") or
/// wizard_reconfigure ("change settings").
class WizardScreen extends StatefulWidget {
  const WizardScreen({super.key, required this.hub, required this.start, required this.title});

  final HubStore hub;
  final Map<String, dynamic> start;
  final String title;

  @override
  State<WizardScreen> createState() => _WizardScreenState();
}

/// Steps the hub answers only once the device or the adapter is done
/// (wizard.rs): sent right away, shown until the reply comes.
const _waitingSteps = {'confirm_on_device', 'provision_ble', 'test'};

class _WizardScreenState extends State<WizardScreen> {
  Map<String, dynamic>? _view;
  bool _busy = true;
  String? _error;
  String? _errorField;
  String? _errorDetail;
  bool _ended = false;
  bool _done = false;
  // Back was pressed: a step that runs by itself (test, waiting for the
  // device) is skipped on the way back -- it would run at once and lead
  // forward again to where Back was pressed (as on the hub, #95).
  bool _goingBack = false;
  final Map<String, TextEditingController> _text = {};
  final Map<String, String> _picked = {};

  String? get _session => _view?['session'] as String?;

  @override
  void initState() {
    super.initState();
    _send(widget.start);
  }

  @override
  void dispose() {
    for (final c in _text.values) {
      c.dispose();
    }
    final s = _session;
    // Left halfway: end the session on the hub too.
    if (s != null && !_done && !_ended) {
      widget.hub.request({'action': 'wizard_cancel', 'session': s}).catchError((_) => <String, dynamic>{});
    }
    super.dispose();
  }

  Duration _timeoutFor(Map<String, dynamic>? view) {
    final t = view?['timeout_s'];
    return Duration(seconds: (t is num ? t.toInt() : 60) + 30);
  }

  Future<void> _send(Map<String, dynamic> request) async {
    setState(() {
      _busy = true;
      _error = null;
      _errorField = null;
      _errorDetail = null;
    });
    try {
      final timeout = switch (_view?['step']) {
        final String s when _waitingSteps.contains(s) => _timeoutFor(_view),
        // A vendor's cloud: the hub allows several requests of 20 s each
        // (wizard.rs, CLOUD_TIMEOUT * 4).
        'vendor_login' => const Duration(seconds: 100),
        _ => const Duration(seconds: 45),
      };
      final reply = await widget.hub.request(request, timeout: timeout);
      if (!mounted) return;
      switch (reply['type']) {
        case 'wizard_step':
          _showStep(reply);
        case 'wizard_error':
          setState(() {
            _error = reply['message'] as String?;
            _errorField = reply['field'] as String?;
            final d = reply['detail'];
            _errorDetail = d is String && d.isNotEmpty ? d : null;
            if ((reply['session'] as String? ?? '').isEmpty) _ended = true;
          });
        case 'device':
          _done = true;
          final device = reply['device'] as Map<String, dynamic>?;
          final nav = Navigator.of(context);
          nav.pop();
          if (device != null && widget.start['action'] == 'wizard_start') {
            nav.push(MaterialPageRoute(builder: (_) => DeviceScreen(hub: widget.hub, id: device['id'] as String)));
          }
          return;
        default:
          setState(() => _error = 'Unexpected answer from the hub (${reply['type']}).');
      }
    } on HubException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  void _back() {
    _goingBack = true;
    _send({'action': 'wizard_back', 'session': _session});
  }

  void _showStep(Map<String, dynamic> view) {
    if (_goingBack && _waitingSteps.contains(view['step'])) {
      // Further back, past it. With nothing before it the hub answers with
      // an error, shown as usual.
      _send({'action': 'wizard_back', 'session': view['session']});
      return;
    }
    _goingBack = false;
    // Fresh inputs for the new step, pre-filled with what the hub sends
    // (an earlier answer, a found device, a default -- never a secret).
    for (final c in _text.values) {
      c.dispose();
    }
    _text.clear();
    _picked.clear();
    for (final f in _fields(view)) {
      final id = f['id'] as String;
      final value = f['value'] as String? ?? '';
      if (f['type'] == 'choice' || f['type'] == 'toggle') {
        _picked[id] = value.isNotEmpty ? value : (f['type'] == 'toggle' ? 'false' : '');
      } else {
        _text[id] = TextEditingController(text: value);
      }
    }
    setState(() => _view = view);
    if (_waitingSteps.contains(view['step'])) {
      // Start waiting at once; the reply is the next step.
      scheduleMicrotask(() => _answer({}));
    }
  }

  static List<Map<String, dynamic>> _fields(Map<String, dynamic> view) => switch (view['step']) {
        'form' => (view['fields'] as List).cast<Map<String, dynamic>>(),
        'choice' || 'code_from_device' => [view['field'] as Map<String, dynamic>],
        'vendor_login' => ((view['fields'] as List?) ?? const []).cast<Map<String, dynamic>>(),
        _ => const [],
      };

  void _answer(Map<String, dynamic> values) =>
      _send({'action': 'wizard_answer', 'session': _session, 'values': values});

  Map<String, String> _collect() => {
        for (final e in _text.entries) e.key: e.value.text.trim(),
        ..._picked,
      };

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final view = _view;
    final number = view?['number'];
    return Scaffold(
      appBar: AppBar(
        title: Text(number == null ? widget.title : '${widget.title} · step $number'),
        actions: [
          if (view != null && (number as num? ?? 1) > 1 && !_busy && !_ended)
            TextButton(onPressed: _back, child: const Text('Back')),
        ],
      ),
      body: SafeArea(
        child: ListView(
          padding: EdgeInsets.all(sp.xl),
          children: [
            if (view == null && _busy) const Center(child: CircularProgressIndicator()),
            if (view != null) _step(context, view),
            if (_error != null) ...[
              SizedBox(height: sp.l),
              _ErrorBox(message: _error!, detail: _errorDetail, field: _fieldLabel(_errorField)),
            ],
            if (_ended) ...[
              SizedBox(height: sp.l),
              FilledButton(onPressed: () => Navigator.of(context).pop(), child: const Text('Close')),
            ],
          ],
        ),
      ),
    );
  }

  String? _fieldLabel(String? id) {
    if (id == null || _view == null) return null;
    for (final f in _fields(_view!)) {
      if (f['id'] == id) return f['label'] as String?;
    }
    return null;
  }

  Widget _step(BuildContext context, Map<String, dynamic> v) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final next = FilledButton(onPressed: _busy || _ended ? null : () => _answer(_collect()), child: const Text('Next'));
    switch (v['step']) {
      case 'info':
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text(v['text'] as String? ?? '', style: theme.textTheme.bodyLarge),
          SizedBox(height: sp.xl),
          next,
        ]);
      case 'discover':
        final found = (v['found'] as List).cast<Map<String, dynamic>>();
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text('Found on your network', style: theme.textTheme.titleMedium),
          SizedBox(height: sp.s),
          if (found.isEmpty) Text('Nothing found yet. Is it switched on and on the same network?', style: theme.textTheme.bodySmall),
          for (final f in found)
            Card(
              child: ListTile(
                title: Text(f['name'] as String? ?? ''),
                subtitle: Text(f['address'] as String? ?? ''),
                trailing: const Icon(Icons.chevron_right),
                onTap: _busy ? null : () => _answer({'found': f['address'] as String}),
              ),
            ),
          SizedBox(height: sp.l),
          OutlinedButton.icon(
            onPressed: _busy ? null : () => _answer({}),
            icon: _busy ? const SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2)) : const Icon(Icons.refresh),
            label: const Text('Search again'),
          ),
        ]);
      case 'form':
      case 'choice':
      case 'code_from_device':
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          for (final f in _fields(v)) ...[_field(context, f), SizedBox(height: sp.l)],
          next,
        ]);
      case 'confirm_on_device':
      case 'provision_ble':
        final hints = ((v['hints'] as List?) ?? const []).cast<String>();
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text(v['hint'] as String? ?? '', style: theme.textTheme.bodyLarge),
          for (final h in hints) Padding(padding: EdgeInsets.only(top: sp.s), child: Text('•  $h')),
          SizedBox(height: sp.xl),
          if (_busy) ...[
            const Center(child: CircularProgressIndicator()),
            SizedBox(height: sp.s),
            Center(child: Text('Waiting up to ${v['timeout_s']} s…', style: theme.textTheme.bodySmall)),
          ] else if (!_ended)
            FilledButton(onPressed: () => _answer({}), child: const Text('Try again')),
        ]);
      case 'test':
        return Column(children: [
          if (_busy) ...[const CircularProgressIndicator(), SizedBox(height: sp.m), const Text('Testing the connection…')]
          else if (!_ended)
            FilledButton(onPressed: () => _answer({}), child: const Text('Test again')),
        ]);
      case 'vendor_login':
        return _vendorLogin(context, v, next);
      case 'name':
        _text.putIfAbsent('name', () => TextEditingController(text: v['name'] as String? ?? ''));
        _text.putIfAbsent('room', () => TextEditingController(text: v['room'] as String? ?? ''));
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          if ((v['summary'] as String? ?? '').isNotEmpty) ...[
            Text('Found: ${v['summary']}', style: theme.textTheme.bodySmall),
            SizedBox(height: sp.l),
          ],
          TextField(controller: _text['name'], decoration: const InputDecoration(labelText: 'Name')),
          SizedBox(height: sp.l),
          Autocomplete<String>(
            initialValue: TextEditingValue(text: _text['room']!.text),
            optionsBuilder: (t) => widget.hub.rooms.where((r) => r.isNotEmpty && r.toLowerCase().contains(t.text.toLowerCase())),
            onSelected: (r) => _text['room']!.text = r,
            fieldViewBuilder: (context, c, focus, _) => TextField(
              controller: c,
              focusNode: focus,
              decoration: const InputDecoration(labelText: 'Room'),
              onChanged: (t) => _text['room']!.text = t,
            ),
          ),
          SizedBox(height: sp.xl),
          FilledButton(
            onPressed: _busy
                ? null
                : () => _send({
                      'action': 'wizard_finish',
                      'session': _session,
                      'name': _text['name']!.text.trim(),
                      'room': _text['room']!.text.trim(),
                    }),
            child: const Text('Add device'),
          ),
        ]);
      case 'save':
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          if ((v['summary'] as String? ?? '').isNotEmpty) Text('Found: ${v['summary']}', style: theme.textTheme.bodySmall),
          SizedBox(height: sp.l),
          Text('Save the changes to “${v['device']}”?', style: theme.textTheme.bodyLarge),
          SizedBox(height: sp.xl),
          FilledButton(
            onPressed: _busy ? null : () => _send({'action': 'wizard_finish', 'session': _session}),
            child: const Text('Save'),
          ),
        ]);
      default:
        // A newer hub's step this app doesn't know: say so instead of guessing.
        return Text('This step (“${v['step']}”) needs a newer version of the app. Use the hub\'s screen to finish.',
            style: theme.textTheme.bodyLarge);
    }
  }

  /// A vendor account (#74, wizard.rs's vendor_login): one step of the
  /// hub in up to three screens ("phase") -- the account (email,
  /// password; or one already saved on the hub), the one-time code the
  /// vendor sent, then the account's devices to pick from. The password
  /// and the code only pass through the hub: it keeps the session, never
  /// the password.
  Widget _vendorLogin(BuildContext context, Map<String, dynamic> v, Widget next) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    final hint = v['hint'] as String? ?? '';
    final phase = v['phase'] as String? ?? 'account';
    final accounts = ((v['accounts'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final devices = ((v['devices'] as List?) ?? const []).cast<Map<String, dynamic>>();
    final startOver = TextButton(
      onPressed: _busy ? null : () => _answer({'restart': true}),
      child: const Text('Use another account'),
    );
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      if (hint.isNotEmpty) ...[Text(hint, style: theme.textTheme.bodyLarge), SizedBox(height: sp.l)],
      if (phase == 'pick') ...[
        if (devices.isEmpty) Text('No devices of this kind on that account.', style: theme.textTheme.bodySmall),
        for (final d in devices)
          Card(
            child: ListTile(
              title: Text(d['name'] as String? ?? d['id'] as String),
              subtitle: (d['detail'] as String? ?? '').isEmpty ? null : Text(d['detail'] as String),
              trailing: const Icon(Icons.chevron_right),
              // An unavailable one is still offered: the hub says why.
              enabled: !_busy,
              onTap: () => _answer({'device': d['id'] as String}),
            ),
          ),
        SizedBox(height: sp.l),
        startOver,
      ] else ...[
        // Accounts the hub is signed in to already: one tap, no password.
        // The screen's own choices (a vacuum's "show the map") still count.
        if (phase == 'account' && accounts.isNotEmpty) ...[
          for (final a in accounts)
            Card(
              child: ListTile(
                leading: const Icon(Icons.account_circle_outlined),
                title: Text(a['label'] as String? ?? a['id'] as String),
                subtitle: const Text('Signed in: use it'),
                trailing: const Icon(Icons.chevron_right),
                enabled: !_busy,
                onTap: () => _answer({
                  'account': a['id'] as String,
                  for (final f in _fields(v))
                    if (f['type'] == 'toggle' || f['type'] == 'choice') f['id'] as String: _picked[f['id']] ?? '',
                }),
              ),
            ),
          SizedBox(height: sp.m),
          Text('Or sign in with another account:', style: theme.textTheme.bodySmall),
          SizedBox(height: sp.m),
        ],
        for (final f in _fields(v)) ...[_field(context, f), SizedBox(height: sp.l)],
        next,
        if (phase == 'code') ...[SizedBox(height: sp.s), startOver],
      ],
    ]);
  }

  Widget _field(BuildContext context, Map<String, dynamic> f) {
    final id = f['id'] as String;
    final label = '${f['label']}${f['required'] == true ? '' : ' (optional)'}';
    final hint = (f['hint'] as String? ?? '').isEmpty ? null : f['hint'] as String;
    final hasError = _errorField == id;
    switch (f['type']) {
      case 'choice':
        final choices = ((f['choices'] as List?) ?? const []).cast<Map<String, dynamic>>();
        return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(label, style: Theme.of(context).textTheme.titleSmall),
          if (hint != null) Text(hint, style: Theme.of(context).textTheme.bodySmall),
          RadioGroup<String>(
            groupValue: _picked[id],
            onChanged: (v) => setState(() => _picked[id] = v ?? ''),
            child: Column(children: [
              for (final c in choices)
                RadioListTile<String>(
                  value: c['value'] as String,
                  title: Text(c['label'] as String? ?? c['value'] as String),
                  contentPadding: EdgeInsets.zero,
                ),
            ]),
          ),
        ]);
      case 'toggle':
        return SwitchListTile(
          title: Text(label),
          subtitle: hint == null ? null : Text(hint),
          value: _picked[id] == 'true',
          contentPadding: EdgeInsets.zero,
          onChanged: (v) => setState(() => _picked[id] = '$v'),
        );
      default:
        final secret = f['type'] == 'secret';
        return TextField(
          controller: _text[id],
          obscureText: secret,
          autocorrect: !secret,
          keyboardType: f['type'] == 'number' ? const TextInputType.numberWithOptions(decimal: true) : TextInputType.text,
          decoration: InputDecoration(
            labelText: label,
            helperText: hint ?? (secret ? 'Kept on the hub, never shown again' : null),
            helperMaxLines: 3,
            errorText: hasError ? _error : null,
          ),
        );
    }
  }
}

class _ErrorBox extends StatelessWidget {
  const _ErrorBox({required this.message, this.detail, this.field});
  final String message;
  final String? detail;
  final String? field;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      padding: EdgeInsets.all(Spacing.of(context).l),
      decoration: BoxDecoration(color: scheme.error.withValues(alpha: 0.12), borderRadius: BorderRadius.circular(8)),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text(field == null ? message : '$field: $message', style: TextStyle(color: scheme.error)),
        if (detail != null) Text(detail!, style: Theme.of(context).textTheme.bodySmall),
      ]),
    );
  }
}
