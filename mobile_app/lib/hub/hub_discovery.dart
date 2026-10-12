import 'dart:async';
import 'dart:io';

import 'package:nsd/nsd.dart' as nsd;

import 'hub_address.dart';

/// Finding hubs on the home network: the hub announces its LAN API as
/// _uchub._tcp over mDNS (yocto: hub-wifi's uchub.service). Finding one
/// only says where to connect -- never who it is: that's still the pinned
/// certificate (pairing compares it, every later connection checks it).
const hubServiceType = '_uchub._tcp';

class FoundHub {
  FoundHub({required this.name, required this.host, required this.port});

  /// What it announces itself as ("stm32mp1").
  final String name;

  /// An IPv4 address when the lookup gave one (works on every phone),
  /// else the .local name.
  final String host;
  final int port;

  @override
  String toString() => port == lanPort ? host : '$host:$port';
}

/// Browses for [within] and returns what answered. Empty when nothing did,
/// or when mDNS isn't available (a simulator, a network blocking multicast).
Future<List<FoundHub>> findHubs({Duration within = const Duration(seconds: 4)}) async {
  nsd.Discovery? discovery;
  try {
    discovery = await nsd.startDiscovery(hubServiceType, ipLookupType: nsd.IpLookupType.v4);
    await Future<void>.delayed(within);
    final found = <String, FoundHub>{};
    for (final s in discovery.services) {
      final addresses = s.addresses ?? const <InternetAddress>[];
      final ipv4 = addresses.where((a) => a.type == InternetAddressType.IPv4);
      final host = ipv4.isNotEmpty ? ipv4.first.address : (addresses.isNotEmpty ? addresses.first.address : s.host);
      if (host == null || host.isEmpty) continue;
      found[host] = FoundHub(name: s.name ?? host, host: host, port: s.port ?? lanPort);
    }
    return found.values.toList();
  } catch (_) {
    return const [];
  } finally {
    if (discovery != null) {
      try {
        await nsd.stopDiscovery(discovery);
      } catch (_) {}
    }
  }
}
