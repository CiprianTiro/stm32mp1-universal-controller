import 'package:flutter/material.dart';

import '../automations/editors.dart';
import '../automations/model.dart';
import '../hub/hub_store.dart';
import '../theme/tokens.dart';
import '../widgets/capability_controls.dart';
import 'home_screen.dart' show sceneIcon;

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

/// Scenes (#47): named lists of steps, run with one tap -- here, on the
/// hub's screen, from the cloud, or by an automation.
class ScenesScreen extends StatelessWidget {
  const ScenesScreen({super.key, required this.hub});
  final HubStore hub;

  Future<void> _capture(BuildContext context) async {
    final name = TextEditingController();
    final picked = <String>{};
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setState) => AlertDialog(
          title: const Text('Save the current state'),
          scrollable: true,
          content: Column(mainAxisSize: MainAxisSize.min, children: [
            TextField(controller: name, decoration: const InputDecoration(labelText: 'Scene name'), autofocus: true),
            const SizedBox(height: 8),
            const Text('Devices to include, as they are right now:'),
            for (final d in hub.devices.where((d) => settableCapabilities.any(d.has)))
              CheckboxListTile(
                contentPadding: EdgeInsets.zero,
                title: Text(d.name),
                subtitle: d.room.isEmpty ? null : Text(d.room),
                value: picked.contains(d.id),
                onChanged: (v) => setState(() => v == true ? picked.add(d.id) : picked.remove(d.id)),
              ),
          ]),
          actions: [
            TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
            FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Save')),
          ],
        ),
      ),
    );
    if (ok == true && context.mounted) {
      await runRequest(context, () => hub.captureScene(name.text.trim(), picked.toList()));
    }
    name.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return ListenableBuilder(
      listenable: hub,
      builder: (context, _) => Scaffold(
        appBar: AppBar(title: const Text('Scenes'), actions: [
          IconButton(tooltip: 'Save the current state', icon: const Icon(Icons.photo_camera_outlined), onPressed: () => _capture(context)),
        ]),
        floatingActionButton: FloatingActionButton.extended(
          onPressed: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => SceneEditor(hub: hub))),
          icon: const Icon(Icons.add),
          label: const Text('New scene'),
        ),
        body: hub.scenes.isEmpty
            ? Center(child: Text('No scenes yet.', style: Theme.of(context).textTheme.bodySmall))
            : GridView.builder(
                padding: EdgeInsets.fromLTRB(sp.xl, sp.l, sp.xl, 96),
                gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
                  maxCrossAxisExtent: 220,
                  mainAxisSpacing: 12,
                  crossAxisSpacing: 12,
                  childAspectRatio: 1.05,
                ),
                itemCount: hub.scenes.length,
                itemBuilder: (context, i) => _SceneCard(hub: hub, scene: hub.scenes[i]),
              ),
      ),
    );
  }
}

/// A scene as a card: its icon, name and steps; Run right on it.
class _SceneCard extends StatelessWidget {
  const _SceneCard({required this.hub, required this.scene});
  final HubStore hub;
  final Map<String, dynamic> scene;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final name = scene['name'] as String;
    return Card(
      child: InkWell(
        onTap: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => SceneEditor(hub: hub, scene: scene))),
        child: Padding(
          padding: const EdgeInsets.all(14),
          child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Container(
              width: 44,
              height: 44,
              decoration: BoxDecoration(color: scheme.primary.withValues(alpha: 0.14), shape: BoxShape.circle),
              child: Icon(sceneIcon(name), color: scheme.primary),
            ),
            const Spacer(),
            Text(name, maxLines: 2, overflow: TextOverflow.ellipsis, style: theme.textTheme.labelLarge),
            Text('${(scene['steps'] as List).length} steps', style: theme.textTheme.bodySmall?.copyWith(fontSize: 13)),
            const SizedBox(height: 8),
            SizedBox(
              width: double.infinity,
              child: FilledButton.tonal(
                style: FilledButton.styleFrom(minimumSize: const Size(0, 36), shape: const StadiumBorder()),
                onPressed: () => runRequest(context, () async {
                  await hub.runScene(scene['id'] as String);
                  if (context.mounted) ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text('$name: done')));
                }),
                child: const Text('Run'),
              ),
            ),
          ]),
        ),
      ),
    );
  }
}

class SceneEditor extends StatefulWidget {
  const SceneEditor({super.key, required this.hub, this.scene});
  final HubStore hub;
  final Map<String, dynamic>? scene;

  @override
  State<SceneEditor> createState() => _SceneEditorState();
}

class _SceneEditorState extends State<SceneEditor> {
  late final TextEditingController _name = TextEditingController(text: widget.scene?['name'] as String? ?? '');
  late final List<Map<String, dynamic>> _steps =
      [for (final s in (widget.scene?['steps'] as List?) ?? const []) Map<String, dynamic>.from(s as Map)];
  bool _saving = false;

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    setState(() => _saving = true);
    await runRequest(context, () async {
      await widget.hub.saveScene({'id': widget.scene?['id'] ?? '', 'name': _name.text.trim(), 'steps': _steps});
      if (mounted) Navigator.of(context).pop();
    });
    if (mounted) setState(() => _saving = false);
  }

  Future<void> _delete() async {
    final ok = await _confirm(context, 'Delete ${widget.scene!['name']}?', 'Automations running it will report an error.');
    if (ok && mounted) {
      await runRequest(context, () async {
        await widget.hub.deleteScene(widget.scene!['id'] as String);
        if (mounted) Navigator.of(context).pop();
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.scene == null ? 'New scene' : 'Edit scene'),
        actions: [
          if (widget.scene != null) IconButton(tooltip: 'Delete', icon: const Icon(Icons.delete_outline), onPressed: _delete),
          TextButton(onPressed: _saving ? null : _save, child: const Text('Save')),
        ],
      ),
      body: ListView(padding: EdgeInsets.all(sp.l), children: [
        TextField(controller: _name, decoration: const InputDecoration(labelText: 'Name')),
        SizedBox(height: sp.xl),
        _ItemList(
          title: 'Steps',
          hint: 'Carried out in this order.',
          items: _steps,
          describe: (s) => describeStep(widget.hub, s),
          onAdd: () async {
            final s = await editStep(context, widget.hub, allowScene: false);
            if (s != null) setState(() => _steps.add(s));
          },
          onEdit: (i) async {
            final s = await editStep(context, widget.hub, initial: _steps[i], allowScene: false);
            if (s != null) setState(() => _steps[i] = s);
          },
          onChanged: () => setState(() {}),
        ),
      ]),
    );
  }
}

// ---------------------------------------------------------------------------
// Automations
// ---------------------------------------------------------------------------

/// Automations (#47): WHEN a trigger happens, IF the conditions hold, DO
/// the steps. All run on the hub, without the phone or the internet.
class AutomationsScreen extends StatelessWidget {
  const AutomationsScreen({super.key, required this.hub});
  final HubStore hub;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    return ListenableBuilder(
      listenable: hub,
      builder: (context, _) => Scaffold(
        appBar: AppBar(title: const Text('Automations'), actions: [
          IconButton(
            tooltip: 'What ran',
            icon: const Icon(Icons.history),
            onPressed: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => LogScreen(hub: hub))),
          ),
        ]),
        floatingActionButton: FloatingActionButton.extended(
          onPressed: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => AutomationEditor(hub: hub))),
          icon: const Icon(Icons.add),
          label: const Text('New automation'),
        ),
        body: hub.automations.isEmpty
            ? Center(child: Text('No automations yet.', style: Theme.of(context).textTheme.bodySmall))
            : ListView(
                padding: EdgeInsets.fromLTRB(sp.xl, sp.l, sp.xl, 96),
                children: [
                  for (final a in hub.automations)
                    Padding(
                      padding: EdgeInsets.only(bottom: sp.s),
                      child: Card(
                        child: ListTile(
                          contentPadding: const EdgeInsets.fromLTRB(14, 8, 8, 8),
                          leading: _TriggerIcon(automation: a),
                          title: Text(a['name'] as String, style: Theme.of(context).textTheme.labelLarge),
                          subtitle: Text(
                            ((a['triggers'] as List).cast<Map<String, dynamic>>()).map((t) => describeTrigger(hub, t)).join(' · or '),
                            maxLines: 2,
                            overflow: TextOverflow.ellipsis,
                          ),
                          onTap: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => AutomationEditor(hub: hub, automation: a))),
                          trailing: Switch(
                            value: a['enabled'] != false,
                            onChanged: (v) => runRequest(context, () => hub.setAutomationEnabled(a['id'] as String, v)),
                          ),
                        ),
                      ),
                    ),
                ],
              ),
      ),
    );
  }
}

/// What starts an automation, as one icon: a clock, the sun, a device.
class _TriggerIcon extends StatelessWidget {
  const _TriggerIcon({required this.automation});
  final Map<String, dynamic> automation;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final triggers = (automation['triggers'] as List).cast<Map<String, dynamic>>();
    final first = triggers.isEmpty ? null : triggers.first;
    final icon = switch (first?['type']) {
      'time' => Icons.schedule_rounded,
      'sun' => first?['event'] == 'sunrise' ? Icons.wb_twilight_rounded : Icons.nights_stay_rounded,
      'state' => Icons.toggle_on_rounded,
      'threshold' => Icons.show_chart_rounded,
      _ => Icons.bolt_rounded,
    };
    final on = automation['enabled'] != false;
    return Container(
      width: 44,
      height: 44,
      decoration: BoxDecoration(color: (on ? scheme.primary : scheme.onSurfaceVariant).withValues(alpha: 0.14), shape: BoxShape.circle),
      child: Icon(icon, color: on ? scheme.primary : scheme.onSurfaceVariant),
    );
  }
}

class AutomationEditor extends StatefulWidget {
  const AutomationEditor({super.key, required this.hub, this.automation});
  final HubStore hub;
  final Map<String, dynamic>? automation;

  @override
  State<AutomationEditor> createState() => _AutomationEditorState();
}

class _AutomationEditorState extends State<AutomationEditor> {
  late final TextEditingController _name = TextEditingController(text: widget.automation?['name'] as String? ?? '');
  List<Map<String, dynamic>> _copy(String key) =>
      [for (final x in (widget.automation?[key] as List?) ?? const []) Map<String, dynamic>.from(x as Map)];
  late final _triggers = _copy('triggers');
  late final _conditions = _copy('conditions');
  late final _steps = _copy('steps');
  bool _saving = false;

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    setState(() => _saving = true);
    await runRequest(context, () async {
      // The hub checks it exactly as it would run it, and says what's wrong.
      await widget.hub.saveAutomation({
        'id': widget.automation?['id'] ?? '',
        'name': _name.text.trim(),
        'enabled': widget.automation?['enabled'] ?? true,
        'triggers': _triggers,
        'conditions': _conditions,
        'steps': _steps,
      });
      if (mounted) Navigator.of(context).pop();
    });
    if (mounted) setState(() => _saving = false);
  }

  Future<void> _delete() async {
    final ok = await _confirm(context, 'Delete ${widget.automation!['name']}?', null);
    if (ok && mounted) {
      await runRequest(context, () async {
        await widget.hub.deleteAutomation(widget.automation!['id'] as String);
        if (mounted) Navigator.of(context).pop();
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final hub = widget.hub;
    final existing = widget.automation;
    return Scaffold(
      appBar: AppBar(
        title: Text(existing == null ? 'New automation' : 'Edit automation'),
        actions: [
          if (existing != null)
            PopupMenuButton<String>(
              onSelected: (v) => switch (v) {
                'run' => runRequest(context, () async {
                    await hub.runAutomation(existing['id'] as String);
                    if (context.mounted) ScaffoldMessenger.of(context).showSnackBar(const SnackBar(content: Text('Ran its steps')));
                  }),
                'delete' => _delete(),
                _ => null,
              },
              itemBuilder: (_) => const [
                PopupMenuItem(value: 'run', child: Text('Run its steps now (test)')),
                PopupMenuItem(value: 'delete', child: Text('Delete…')),
              ],
            ),
          TextButton(onPressed: _saving ? null : _save, child: const Text('Save')),
        ],
      ),
      body: ListView(padding: EdgeInsets.all(sp.l), children: [
        TextField(controller: _name, decoration: const InputDecoration(labelText: 'Name')),
        SizedBox(height: sp.xl),
        _ItemList(
          title: 'When',
          hint: 'Any one of these starts it.',
          items: _triggers,
          describe: (t) => describeTrigger(hub, t),
          onAdd: () async {
            final t = await editTrigger(context, hub);
            if (t != null) setState(() => _triggers.add(t));
          },
          onEdit: (i) async {
            final t = await editTrigger(context, hub, _triggers[i]);
            if (t != null) setState(() => _triggers[i] = t);
          },
          onChanged: () => setState(() {}),
        ),
        SizedBox(height: sp.xl),
        _ItemList(
          title: 'Only if',
          hint: 'All of these must hold (none: always).',
          items: _conditions,
          describe: (c) => describeCondition(hub, c),
          onAdd: () async {
            final c = await editCondition(context, hub);
            if (c != null) setState(() => _conditions.add(c));
          },
          onEdit: (i) async {
            final c = await editCondition(context, hub, _conditions[i]);
            if (c != null) setState(() => _conditions[i] = c);
          },
          onChanged: () => setState(() {}),
        ),
        SizedBox(height: sp.xl),
        _ItemList(
          title: 'Do',
          hint: 'Carried out in this order.',
          items: _steps,
          describe: (s) => describeStep(hub, s),
          onAdd: () async {
            final s = await editStep(context, hub, allowScene: true);
            if (s != null) setState(() => _steps.add(s));
          },
          onEdit: (i) async {
            final s = await editStep(context, hub, initial: _steps[i], allowScene: true);
            if (s != null) setState(() => _steps[i] = s);
          },
          onChanged: () => setState(() {}),
        ),
      ]),
    );
  }
}

/// What ran since the hub started (#47): scenes and automations, why, and
/// whether every step worked. Live while open.
class LogScreen extends StatefulWidget {
  const LogScreen({super.key, required this.hub});
  final HubStore hub;

  @override
  State<LogScreen> createState() => _LogScreenState();
}

class _LogScreenState extends State<LogScreen> {
  String? _error;

  @override
  void initState() {
    super.initState();
    widget.hub.loadLog().catchError((Object e) {
      if (mounted) setState(() => _error = '$e');
    });
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(title: const Text('What ran')),
      body: ListenableBuilder(
        listenable: widget.hub,
        builder: (context, _) {
          final log = widget.hub.log;
          if (_error != null) return Center(child: Text(_error!));
          if (log == null) return const Center(child: CircularProgressIndicator());
          if (log.isEmpty) return Center(child: Text('Nothing has run since the hub started.', style: theme.textTheme.bodySmall));
          return ListView(children: [
            for (final e in log)
              ListTile(
                leading: Icon(
                  e['ok'] == true ? Icons.check_circle_outline : Icons.error_outline,
                  color: e['ok'] == true ? theme.colorScheme.primary : theme.colorScheme.error,
                ),
                title: Text('${e['name']}  ·  ${e['kind']}'),
                subtitle: Text([
                  _when(e['at']),
                  '${e['cause']}',
                  if ((e['detail'] as String? ?? '').isNotEmpty) '${e['detail']}',
                ].join('\n')),
                isThreeLine: (e['detail'] as String? ?? '').isNotEmpty,
              ),
          ]);
        },
      ),
    );
  }

  /// In the phone's time zone -- normally the hub's too (same house).
  String _when(Object? at) {
    if (at is! num) return '';
    final t = DateTime.fromMillisecondsSinceEpoch(at.toInt() * 1000);
    final today = DateTime.now();
    final day = t.year == today.year && t.month == today.month && t.day == today.day ? 'today' : '${t.day}.${t.month}.';
    return '$day ${hhmm(t.hour, t.minute)}';
  }
}

// ---------------------------------------------------------------------------

class _ItemList extends StatelessWidget {
  const _ItemList({
    required this.title,
    required this.hint,
    required this.items,
    required this.describe,
    required this.onAdd,
    required this.onEdit,
    required this.onChanged,
  });

  final String title, hint;
  final List<Map<String, dynamic>> items;
  final String Function(Map<String, dynamic>) describe;
  final VoidCallback onAdd;
  final void Function(int) onEdit;
  final VoidCallback onChanged;

  @override
  Widget build(BuildContext context) {
    final sp = Spacing.of(context);
    final theme = Theme.of(context);
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      Text(title, style: theme.textTheme.titleMedium),
      Text(hint, style: theme.textTheme.bodySmall),
      SizedBox(height: sp.s),
      ReorderableListView(
        shrinkWrap: true,
        physics: const NeverScrollableScrollPhysics(),
        buildDefaultDragHandles: items.length > 1,
        onReorder: (from, to) {
          if (to > from) to--;
          items.insert(to, items.removeAt(from));
          onChanged();
        },
        children: [
          for (var i = 0; i < items.length; i++)
            Card(
              key: ObjectKey(items[i]),
              margin: EdgeInsets.only(bottom: sp.xs),
              child: ListTile(
                title: Text(describe(items[i])),
                onTap: () => onEdit(i),
                trailing: IconButton(
                  icon: const Icon(Icons.close),
                  tooltip: 'Remove',
                  onPressed: () {
                    items.removeAt(i);
                    onChanged();
                  },
                ),
              ),
            ),
        ],
      ),
      Align(alignment: Alignment.centerLeft, child: TextButton.icon(onPressed: onAdd, icon: const Icon(Icons.add), label: const Text('Add'))),
    ]);
  }
}

Future<bool> _confirm(BuildContext context, String title, String? text) async =>
    await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(title),
        content: text == null ? null : Text(text),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Delete')),
        ],
      ),
    ) ??
    false;
