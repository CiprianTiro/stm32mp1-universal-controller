/*
 * oui.rs -- who made a device, from its MAC address (issue #73).
 *
 * The first three bytes of a MAC address are the maker's "OUI", bought
 * from the IEEE. oui_table.rs is a trimmed copy of the IEEE's list (the
 * makers of smart-home things, see tools/gen_oui.py), sorted, so a lookup
 * is a binary search.
 *
 * Two things to know when reading the answer:
 *   - it's the maker of the NETWORK CHIP: a Tuya plug, a Sonoff switch and
 *     a WLED strip all say "Espressif" if they're built on an ESP32;
 *   - phones and some newer devices use a RANDOM ("private") address per
 *     network, marked by one bit of the first byte: it says nothing about
 *     the maker, and we say "private address".
 */
use crate::oui_table::{OUI, VENDORS};

/* The maker for a MAC address "aa:bb:cc:dd:ee:ff" (netscan::normalize_mac);
 * "" if it isn't in the table. */
pub fn manufacturer(mac: &str) -> String {
    let Some(prefix) = u32::from_str_radix(mac.replace(':', "").get(..6).unwrap_or(""), 16).ok() else {
        return String::new();
    };
    /* The "locally administered" bit: set by whoever made the address up,
     * not by a maker. */
    if prefix & 0x02_0000 != 0 {
        return "private address".into();
    }
    match OUI.binary_search_by_key(&prefix, |(oui, _)| *oui) {
        Ok(i) => VENDORS[OUI[i].1 as usize].to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_makers_are_found() {
        assert_eq!(manufacturer("24:0a:c4:11:22:33"), "Espressif");
        assert_eq!(manufacturer("8c:bf:ea:9a:1b:2c"), "Espressif");
        assert_eq!(manufacturer("da:a1:19:00:00:01"), "private address");
        assert_eq!(manufacturer("00:00:01:00:00:01"), "");
        assert_eq!(manufacturer("nonsense"), "");
    }

    #[test]
    fn the_table_is_sorted() {
        assert!(OUI.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(OUI.iter().all(|(_, v)| (*v as usize) < VENDORS.len()));
    }
}
