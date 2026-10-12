import 'package:flutter/material.dart';

import '../hub/hub_store.dart';
import 'automations_screens.dart';
import 'device_screen.dart';
import 'home_screen.dart';

/// The paired app: devices, scenes, automations -- and, under all three,
/// why it isn't connected when it isn't.
class Shell extends StatefulWidget {
  const Shell({super.key, required this.hub});
  final HubStore hub;

  @override
  State<Shell> createState() => _ShellState();
}

class _ShellState extends State<Shell> {
  // Demo mode only (screenshots): --dart-define=DEMO_TAB=1 / DEMO_DEVICE=ac.
  int _tab = const int.fromEnvironment('DEMO_TAB');

  @override
  void initState() {
    super.initState();
    const device = String.fromEnvironment('DEMO_DEVICE');
    if (device.isNotEmpty) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        Navigator.of(context).push(MaterialPageRoute(builder: (_) => DeviceScreen(hub: widget.hub, id: device)));
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final hub = widget.hub;
    return Scaffold(
      body: Column(children: [
        Expanded(
          child: IndexedStack(index: _tab, children: [
            HomeScreen(hub: hub),
            ScenesScreen(hub: hub),
            AutomationsScreen(hub: hub),
          ]),
        ),
        SafeArea(top: false, bottom: false, child: StatusBanner(hub: hub)),
      ]),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: const [
          NavigationDestination(icon: Icon(Icons.home_outlined), selectedIcon: Icon(Icons.home), label: 'Devices'),
          NavigationDestination(icon: Icon(Icons.auto_awesome_outlined), selectedIcon: Icon(Icons.auto_awesome), label: 'Scenes'),
          NavigationDestination(icon: Icon(Icons.schedule_outlined), selectedIcon: Icon(Icons.schedule), label: 'Automations'),
        ],
      ),
    );
  }
}
