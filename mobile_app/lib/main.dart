import 'package:flutter/material.dart';

import 'demo/demo_hub.dart';
import 'hub/hub_store.dart';
import 'screens/shell.dart';
import 'screens/pairing_screen.dart';
import 'theme/tokens.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final tokens = await DesignTokens.load();
  final hub = HubStore();
  // --dart-define=DEMO=true: a pretend hub with a sample house (screenshots,
  // trying the app without a DK2).
  if (const bool.fromEnvironment('DEMO')) {
    await hub.startDemo(DemoHub().handle);
  } else {
    hub.start();
  }
  runApp(HubApp(hub: hub, tokens: tokens));
}

class HubApp extends StatelessWidget {
  const HubApp({super.key, required this.hub, required this.tokens});

  final HubStore hub;
  final DesignTokens tokens;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: hub,
      builder: (context, _) {
        final s = hub.settings;
        return MaterialApp(
          title: 'Universal Controller',
          debugShowCheckedModeBanner: false,
          // The hub's chosen look (#39); it follows a change on the hub at once.
          theme: tokens.theme(
            mode: s['mode'] as String? ?? 'dark',
            accent: s['accent'] as String? ?? 'sky',
            density: s['density'] as String? ?? 'comfortable',
          ),
          home: switch (hub.status) {
            HubStatus.starting => const Scaffold(body: Center(child: CircularProgressIndicator())),
            HubStatus.unpaired => PairingScreen(hub: hub),
            _ => Shell(hub: hub),
          },
        );
      },
    );
  }
}
