# mobile_app — the Universal Controller phone app

Android and iOS from one Flutter codebase (issue #48). It talks to the hub on
the home network only, with no cloud. It uses the same WebSocket API as the
touchscreen (`linux_a7/backend_daemon/src/ws.rs`, protocol v2).

## What it does

| | |
|---|---|
| **Find the hub** | mDNS, `_uchub._tcp`: the hub announces it through `hub-wifi`'s `uchub.service`. The app also finds the hub again by itself when its IP changes. |
| **Pair** (#35) | Scan the QR code on the hub's screen, or type the 6-digit code after comparing the certificate fingerprint. The key is kept in the Android Keystore / iOS Keychain. The hub's certificate is pinned (SHA-256) and checked on every connection before anything is sent. |
| **First-time setup** (#36) | Over Bluetooth: SPAKE2 with the code on the hub's screen (`lib/ble/spake2.dart`, checked byte for byte against python-spake2), then AES-256-GCM. It sends the WiFi details and pairs in the same step. Protocol: `ble.rs`. Reference client: `tools/ble_setup.py`. |
| **Devices** | Controls are built from capabilities, with live updates (`subscribe`). A capability the app doesn't know yet is shown read-only. |
| **Add device** (#40) | The hub's wizard, drawn step by step from what the hub sends: "Found on your network", every template, plus pair again / change settings. |
| **Scenes and automations** (#47) | Run scenes and save the current state as a scene. The automation editor covers every trigger, condition and step kind, plus the log of what ran. |
| **Look** (#39) | `assets/tokens.json` is a **symlink** to `linux_a7/ui_layer/ui/tokens.json`. The app follows the preset chosen on the hub. |

## Build and run

Flutter 3.38 (stable).

```sh
cd mobile_app
flutter pub get
flutter test
flutter run                      # a connected phone
flutter build apk --release      # Android; CI does this on every change
```

CI (`.github/workflows/mobile_app.yml`) analyzes, tests and builds the
Android APK, and keeps it as a build artifact for 14 days.

### iOS

iOS builds need a Mac with Xcode, and an Apple developer account to run on a
phone.

1. `cd mobile_app && flutter pub get && cd ios && pod install`
2. Open `ios/Runner.xcworkspace` in Xcode. Under *Signing & Capabilities*,
   pick your team and change the bundle id (`com.housecontroller.houseControllerApp`)
   to one your team owns.
3. `flutter run` with the iPhone connected, or `flutter build ipa` for TestFlight.

The app asks for these permissions on first use (texts in `ios/Runner/Info.plist`):
local network (finding and talking to the hub), camera (the QR code), and
Bluetooth (first-time setup). Bonjour browsing is declared for `_uchub._tcp`.
Raw multicast would need Apple's restricted multicast entitlement, so the app
uses Bonjour instead.

## Trying it against a hub

- **Pairing:** on the hub, open *Settings → Paired devices → Pair a new device*.
  Then scan the QR code, or pick the hub under *Type the code*.
- **Bluetooth setup:** show the same pairing code, then choose *Set it up over
  Bluetooth* on the pairing screen.
- If the hub's address changes, the app follows it through mDNS. If mDNS is
  blocked on your network, use *⋮ → Change hub address*.
