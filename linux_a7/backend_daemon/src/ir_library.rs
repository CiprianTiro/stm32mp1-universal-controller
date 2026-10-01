/*
 * ir_library.rs -- the IR code library (issue #82): known IR codes by
 * device type and brand, for devices whose remote is lost or broken.
 *
 * WHERE IT COMES FROM. The Flipper Zero community's database
 * (Flipper-IRDB, licence CC0 = public domain), converted during the image
 * build by ir_library/irdb_import.py (recipe ir-library.bb) into
 *
 *   /usr/share/universal-controller/ir-library/index.json
 *   /usr/share/universal-controller/ir-library/<type>.json   e.g. tv.json
 *
 * Read-only data on the rootfs; HUB_IR_LIBRARY_DIR points elsewhere (tests
 * on the PC). A type file is a few hundred KB to ~1 MB, so it's only read
 * when the code finder runs for that type, and only the last one read is
 * kept in memory.
 *
 * A CODE SET is one remote: a list of [button name, code] pairs, the
 * code as the library stores it (ir_encode.rs turns it into what the
 * blaster sends).
 *
 * THE CODE FINDER. For a type and brand, the person is asked "Did the
 * device react?" after the hub sends one harmless button (Power) from a
 * set. Many sets share the same Power code (26 LED strip sets use NEC
 * 0x00/0x40), so the sets are GROUPED by that code: each group is tried
 * once. After "Yes", a second button (the set's CHECK button: a colour,
 * Mute, ...) tells the sets of that group apart, and the set that passes
 * is copied onto the device (ir_codes.rs), like taught buttons.
 */
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::ir_encode;

pub const IR_LIBRARY_DIR: &str = "/usr/share/universal-controller/ir-library";

/* The library's format version (irdb_import.py FORMAT). */
const FORMAT: u32 = 1;

/* The brand name for "I don't know / not listed": all sets of the type. */
pub const ANY_BRAND: &str = "";

/* No-name remotes (irdb_import.py GENERIC): listed first among the brands,
 * since most cheap devices (LED strips) are there. */
const GENERIC: &str = "Generic (no brand)";

#[derive(Deserialize)]
struct Index {
    format: u32,
    types: Vec<IndexType>,
}

#[derive(Deserialize)]
struct IndexType {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct TypeFile {
    format: u32,
    #[serde(rename = "type")]
    type_id: String,
    /* Brand -> its code sets, brands sorted by name. */
    brands: BTreeMap<String, Vec<CodeSet>>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct CodeSet {
    /* Unique in the whole library: "LED_Lighting/Unknown/LED_44Key". */
    pub id: String,
    pub name: String,
    /* [button name, code], in the remote's order. */
    pub buttons: Vec<(String, Value)>,
}

pub struct IrLibrary {
    dir: PathBuf,
    /* The type file read last: the finder asks several times in a row. */
    cache: Mutex<Option<Arc<TypeFile>>>,
}

/* The hub's one library, opened on first use. */
pub fn shared() -> &'static IrLibrary {
    static LIBRARY: OnceLock<IrLibrary> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let dir = std::env::var_os("HUB_IR_LIBRARY_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(IR_LIBRARY_DIR));
        IrLibrary::new(dir)
    })
}

impl IrLibrary {
    pub fn new(dir: PathBuf) -> Self {
        IrLibrary { dir, cache: Mutex::new(None) }
    }

    /* The device types: [{"id": "tv", "name": "TVs"}, ...]. */
    pub fn types(&self) -> Result<Vec<Value>, String> {
        let text = std::fs::read(self.dir.join("index.json")).map_err(|e| self.missing(e))?;
        let index: Index = serde_json::from_slice(&text).map_err(|e| format!("IR code library: index.json: {e}"))?;
        if index.format != FORMAT {
            return Err(format!("IR code library: format {} unknown to this hub", index.format));
        }
        Ok(index.types.into_iter().map(|t| json!({"id": t.id, "name": t.name})).collect())
    }

    /* A type's brands with how many sets this hub can send:
     * [{"name": "LG", "sets": 12}, ...], by name with GENERIC first,
     * brands without a usable set left out. */
    pub fn brands(&self, type_id: &str) -> Result<Vec<Value>, String> {
        let file = self.load(type_id)?;
        let mut brands: Vec<Value> = file
            .brands
            .iter()
            .filter_map(|(brand, sets)| {
                let usable = sets.iter().filter(|s| test_code(s).is_some()).count();
                (usable > 0).then(|| json!({"name": brand, "sets": usable}))
            })
            .collect();
        /* Stable: the others keep their order. */
        brands.sort_by_key(|b| b["name"] != GENERIC);
        Ok(brands)
    }

    /* The finder's list for a type and brand (ANY_BRAND: all brands):
     * groups of sets sharing one test code, most shared first (the most
     * common remote). Inside a group, sets whose check button also sends
     * the same code can't be told apart by the person either: only the
     * one with the most buttons is listed, with "alike" = how many it
     * stands for. */
    pub fn candidates(&self, type_id: &str, brand: &str) -> Result<Vec<Value>, String> {
        let file = self.load(type_id)?;
        let sets: Vec<(&String, &CodeSet)> = if brand == ANY_BRAND {
            file.brands.iter().flat_map(|(b, sets)| sets.iter().map(move |s| (b, s))).collect()
        } else {
            let (brand, sets) = file.brands.get_key_value(brand).ok_or_else(|| format!("no brand {brand:?} for {type_id}"))?;
            sets.iter().map(|s| (brand, s)).collect()
        };

        struct Choice<'a> {
            brand: &'a str,
            set: &'a CodeSet,
            button: String,
            check: Option<String>,
            sendable: usize,
            alike: usize,
        }
        struct Group<'a> {
            button: String,
            total: usize,
            /* Keyed by the check button's code (JSON text; "" = none). */
            choices: Vec<(String, Choice<'a>)>,
        }
        /* Grouped by the code the blaster would send for the test button.
         * (Its JSON text is the key: the same code, the same text.) */
        let mut groups: Vec<Group> = Vec::new();
        let mut where_is: HashMap<String, usize> = HashMap::new();
        for (brand, set) in sets {
            let Some((button, code)) = test_code(set) else { continue };
            let check = check_button(set, &button);
            let check_key = check.as_ref().map(|(_, code)| code.to_string()).unwrap_or_default();
            let sendable = set.buttons.iter().filter(|(_, c)| ir_encode::to_blaster(c).is_some()).count();
            let choice = Choice { brand, set, button: button.clone(), check: check.map(|(name, _)| name), sendable, alike: 1 };
            let i = *where_is.entry(code.to_string()).or_insert_with(|| {
                groups.push(Group { button, total: 0, choices: Vec::new() });
                groups.len() - 1
            });
            let group = &mut groups[i];
            group.total += 1;
            match group.choices.iter_mut().find(|(key, _)| *key == check_key) {
                Some((_, kept)) => {
                    let alike = kept.alike + 1;
                    if choice.sendable > kept.sendable {
                        *kept = choice;
                    }
                    kept.alike = alike;
                }
                None => group.choices.push((check_key, choice)),
            }
        }
        /* Stable sort: equal sizes keep the library's (brand, name) order. */
        groups.sort_by_key(|g| std::cmp::Reverse(g.total));
        Ok(groups
            .into_iter()
            .map(|g| {
                let sets: Vec<Value> = g
                    .choices
                    .into_iter()
                    .map(|(_, c)| {
                        json!({
                            "id": c.set.id,
                            "name": c.set.name,
                            "brand": c.brand,
                            "button": c.button,
                            "check": c.check,
                            "alike": c.alike,
                        })
                    })
                    .collect();
                json!({"button": g.button, "sets": sets})
            })
            .collect())
    }

    /* One set by its id (the type is given so only its file is read). */
    pub fn set(&self, type_id: &str, set_id: &str) -> Result<CodeSet, String> {
        let file = self.load(type_id)?;
        file.brands
            .values()
            .flatten()
            .find(|s| s.id == set_id)
            .cloned()
            .ok_or_else(|| format!("no code set {set_id:?} in {type_id}"))
    }

    fn load(&self, type_id: &str) -> Result<Arc<TypeFile>, String> {
        /* A type id is a plain name: never a path out of the library. */
        if type_id.is_empty() || !type_id.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(format!("no device type {type_id:?} in the IR code library"));
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some(file) = cache.as_ref().filter(|f| f.type_id == type_id) {
            return Ok(file.clone());
        }
        let path = self.dir.join(format!("{type_id}.json"));
        let text = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound if self.dir.join("index.json").exists() => {
                format!("no device type {type_id:?} in the IR code library")
            }
            _ => self.missing(e),
        })?;
        let file: TypeFile = serde_json::from_slice(&text).map_err(|e| format!("IR code library: {type_id}.json: {e}"))?;
        if file.format != FORMAT {
            return Err(format!("IR code library: format {} unknown to this hub", file.format));
        }
        let file = Arc::new(file);
        *cache = Some(file.clone());
        Ok(file)
    }

    fn missing(&self, e: std::io::Error) -> String {
        format!("The IR code library isn't installed on this hub ({}: {e})", self.dir.display())
    }
}

/* Compares button names loosely: "Vol up", "VOL_UP", "vol-up" -> "volup". */
fn plain(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+').flat_map(char::to_lowercase).collect()
}

/* The finder's test button: Power (a toggle, harmless on anything the
 * finder covers, and you see it), else On, else the first button; it
 * must be sendable. Returns its name and the code the blaster sends. */
pub fn test_code(set: &CodeSet) -> Option<(String, Value)> {
    let rank = |name: &str| -> u8 {
        match plain(name).as_str() {
            "power" | "pwr" | "onoff" | "poweronoff" | "powertoggle" | "standby" => 0,
            p if p.starts_with("power") && !p.contains("off") => 1,
            "on" | "lighton" | "turnon" => 2,
            p if p.contains("power") => 3,
            _ => 4,
        }
    };
    set.buttons
        .iter()
        .filter_map(|(name, code)| Some((rank(name), name, ir_encode::to_blaster(code)?)))
        .min_by_key(|(rank, _, _)| *rank)
        .map(|(_, name, code)| (name.clone(), code))
}

/* The second button, to tell apart the sets that share a Power code:
 * something with a visible or audible effect that undoes nothing (a
 * colour, Mute, volume, brightness), else any other sendable button that
 * isn't an off switch. Its name and the code the blaster sends; None: the
 * set has nothing else to try. */
pub fn check_button(set: &CodeSet, test: &str) -> Option<(String, Value)> {
    const LIKED: [&str; 13] =
        ["red", "blue", "green", "white", "mute", "vol+", "volup", "volumeup", "brightness+", "brighter", "up", "menu", "speed"];
    let others: Vec<(&String, Value)> = set
        .buttons
        .iter()
        .filter(|(name, _)| name != test)
        .filter_map(|(name, code)| Some((name, ir_encode::to_blaster(code)?)))
        .collect();
    let pick = LIKED
        .iter()
        .find_map(|liked| others.iter().find(|(n, _)| plain(n) == *liked))
        .or_else(|| others.iter().find(|(n, _)| !plain(n).contains("off") && !plain(n).contains("power")))?;
    Some((pick.0.clone(), pick.1.clone()))
}

/* The device types whose remotes are TV-like: their library buttons get
 * the hub's standard names (device.rs REMOTE_BUTTONS), so a screen lays
 * them out as a remote (arrows around OK, volume, digits) instead of a
 * grid of names. An LED strip's "Red" stays "Red". */
const TV_LIKE: [&str; 10] =
    ["tv", "projector", "av_receiver", "soundbar", "speaker", "streaming", "cable_box", "bluray", "dvd", "monitor"];

/* A set's buttons with standard names where they clearly match ("Vol_up",
 * "VOL+" -> VOLUME_UP; "POWER" -> "Power", which has no standard name but
 * goes on top). The first button for a name wins; a second one ("Vol up"
 * after "VOL+") keeps its own name. Other types: unchanged. */
pub fn standard_names(type_id: &str, buttons: Vec<(String, Value)>) -> Vec<(String, Value)> {
    if !TV_LIKE.contains(&type_id) {
        return buttons;
    }
    let mut used: Vec<String> = Vec::new();
    buttons
        .into_iter()
        .map(|(name, code)| {
            let name = match standard_name(&name) {
                Some(standard) if !used.iter().any(|u| u == standard) => standard.to_string(),
                _ => name,
            };
            used.push(name.clone());
            (name, code)
        })
        .collect()
}

/* "Vol_up" -> Some("VOLUME_UP"). Compared lowercase with only letters,
 * digits, + and - kept ("Vol-" and "Vol+" must stay apart). */
fn standard_name(name: &str) -> Option<&'static str> {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '-').flat_map(char::to_lowercase).collect();
    let n = n.as_str();
    Some(match n {
        "power" | "pwr" | "onoff" | "poweronoff" | "powertoggle" | "standby" => "Power",
        "up" | "arrowup" | "cursorup" | "navup" | "dpadup" => "UP",
        "down" | "dn" | "arrowdown" | "cursordown" | "navdown" | "dpaddown" => "DOWN",
        "left" | "arrowleft" | "cursorleft" | "navleft" | "dpadleft" => "LEFT",
        "right" | "arrowright" | "cursorright" | "navright" | "dpadright" => "RIGHT",
        "ok" | "enter" | "select" | "center" | "dpadcenter" => "OK",
        "back" | "return" => "BACK",
        "home" => "HOME",
        "menu" => "MENU",
        "exit" => "EXIT",
        "info" | "display" => "INFO",
        "vol+" | "volup" | "volumeup" | "volplus" | "volume+" => "VOLUME_UP",
        "vol-" | "voldn" | "voldown" | "volumedown" | "volminus" | "volume-" => "VOLUME_DOWN",
        "mute" => "MUTE",
        "ch+" | "chup" | "channelup" | "chplus" | "channel+" | "prog+" | "p+" => "CHANNEL_UP",
        "ch-" | "chdn" | "chdown" | "channeldown" | "chminus" | "channel-" | "prog-" | "p-" => "CHANNEL_DOWN",
        "play" => "PLAY",
        "pause" => "PAUSE",
        "stop" => "STOP",
        "rew" | "rewind" => "REWIND",
        "ff" | "fastforward" | "fwd" | "forward" => "FAST_FORWARD",
        "red" => "RED",
        "green" => "GREEN",
        "yellow" => "YELLOW",
        "blue" => "BLUE",
        _ => {
            /* Digits: "1", "Num 1", "Key_1". */
            let digit = n.strip_prefix("num").or_else(|| n.strip_prefix("key")).unwrap_or(n);
            return ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"].into_iter().find(|d| *d == digit);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nec(command: u8) -> Value {
        json!({"proto": "nec", "address": 0, "command": command})
    }

    fn set(id: &str, buttons: &[(&str, Value)]) -> Value {
        json!({"id": id, "name": id, "buttons": buttons.iter().map(|(n, c)| json!([n, c])).collect::<Vec<_>>()})
    }

    /* A small library on disk, like irdb_import.py writes it. */
    fn library() -> (tempdir::Dir, IrLibrary) {
        let dir = tempdir::Dir::new();
        std::fs::write(dir.path().join("index.json"), json!({"format": 1, "types": [{"id": "led_lighting", "name": "LED lights"}]}).to_string()).unwrap();
        /* NEC42ext: no encoder on the hub, so this set can't be sent. */
        let unknown = json!({"proto": "nec42ext", "address": 7, "command": 2});
        let file = json!({
            "format": 1, "type": "led_lighting", "name": "LED lights",
            "brands": {
                "Acme": [
                    set("L/Acme/44", &[("POWER", nec(0x40)), ("Red", nec(0x58)), ("OFF", nec(0x41))]),
                    set("L/Acme/Other", &[("Power", unknown.clone())]),
                ],
                "Generic (no brand)": [
                    set("L/Unknown/44b", &[("Light up", nec(0x5C)), ("Power", nec(0x40)), ("Blue", nec(0x59))]),
                    set("L/Unknown/24", &[("ON", nec(0x03)), ("OFF", nec(0x02)), ("Green", nec(0x05))]),
                ],
            }
        });
        std::fs::write(dir.path().join("led_lighting.json"), file.to_string()).unwrap();
        let lib = IrLibrary::new(dir.path().to_path_buf());
        (dir, lib)
    }

    /* A throwaway directory, removed when dropped (no extra crate). */
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("uc-ir-library-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn types_and_brands() {
        let (_dir, lib) = library();
        assert_eq!(lib.types().unwrap(), vec![json!({"id": "led_lighting", "name": "LED lights"})]);
        /* Acme's NEC42ext set can't be sent: only one set counts. */
        assert_eq!(
            lib.brands("led_lighting").unwrap(),
            vec![json!({"name": GENERIC, "sets": 2}), json!({"name": "Acme", "sets": 1})]
        );
        assert!(lib.brands("tv").unwrap_err().contains("no device type"));
        assert!(lib.brands("../etc").unwrap_err().contains("no device type"));
    }

    #[test]
    fn candidates_group_by_test_code() {
        let (_dir, lib) = library();
        let all = lib.candidates("led_lighting", ANY_BRAND).unwrap();
        /* Two sets share NEC 0x40 as Power: one group, tried first. */
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["button"], "POWER");
        let ids: Vec<&str> = all[0]["sets"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["L/Acme/44", "L/Unknown/44b"]);
        assert_eq!(all[0]["sets"][0]["check"], "Red");
        assert_eq!(all[0]["sets"][1]["button"], "Power");
        assert_eq!(all[0]["sets"][1]["check"], "Blue");
        /* The 24-key set has no Power: its ON is the test button. */
        assert_eq!(all[1]["button"], "ON");
        assert_eq!(all[1]["sets"][0]["check"], "Green");
        /* One brand only. */
        let acme = lib.candidates("led_lighting", "Acme").unwrap();
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0]["sets"][0]["brand"], "Acme");
        assert!(lib.candidates("led_lighting", "Nobody").is_err());
    }

    #[test]
    fn look_alike_sets_are_listed_once() {
        let (dir, _) = library();
        /* A copy of Acme's 44-key set with one more button: same test
         * and check codes, so it stands in for both. */
        let mut file: Value = serde_json::from_slice(&std::fs::read(dir.path().join("led_lighting.json")).unwrap()).unwrap();
        file["brands"][GENERIC].as_array_mut().unwrap().push(set(
            "L/Unknown/44c",
            &[("POWER", nec(0x40)), ("Red", nec(0x58)), ("Flash", nec(0x4D)), ("Fade", nec(0x4E))],
        ));
        std::fs::write(dir.path().join("led_lighting.json"), file.to_string()).unwrap();
        let lib = IrLibrary::new(dir.path().to_path_buf());
        let all = lib.candidates("led_lighting", ANY_BRAND).unwrap();
        let first = all[0]["sets"].as_array().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["id"], "L/Unknown/44c");
        assert_eq!(first[0]["alike"], 2);
        assert_eq!(first[1]["id"], "L/Unknown/44b");
        assert_eq!(first[1]["alike"], 1);
    }

    #[test]
    fn tv_buttons_get_standard_names() {
        let b = |names: &[&str]| names.iter().map(|n| (n.to_string(), nec(0))).collect::<Vec<_>>();
        let names = |v: Vec<(String, Value)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(
            names(standard_names("tv", b(&["POWER", "Vol_up", "VOL-", "Vol up", "Ch+", "Num 5", "Netflix", "OK"]))),
            ["Power", "VOLUME_UP", "VOLUME_DOWN", "Vol up", "CHANNEL_UP", "5", "Netflix", "OK"]
        );
        /* Not a TV-like type: as the library has them. */
        assert_eq!(names(standard_names("led_lighting", b(&["Red", "POWER"]))), ["Red", "POWER"]);
    }

    #[test]
    fn set_by_id() {
        let (_dir, lib) = library();
        assert_eq!(lib.set("led_lighting", "L/Unknown/24").unwrap().buttons.len(), 3);
        assert!(lib.set("led_lighting", "L/Unknown/nope").is_err());
    }

    #[test]
    fn missing_library_is_explained() {
        let lib = IrLibrary::new(PathBuf::from("/nonexistent/ir-library"));
        assert!(lib.types().unwrap_err().contains("isn't installed"));
        assert!(lib.brands("tv").unwrap_err().contains("isn't installed"));
    }
}

/* Against a real library made by irdb_import.py, e.g.:
 *   HUB_IR_LIBRARY_DIR=/tmp/irlib cargo test ir_library::real -- --ignored --nocapture */
#[cfg(test)]
mod real {
    use super::*;

    #[test]
    #[ignore]
    fn summary() {
        let lib = shared();
        for t in lib.types().unwrap() {
            let id = t["id"].as_str().unwrap();
            let brands = lib.brands(id).unwrap();
            let sets: u64 = brands.iter().map(|b| b["sets"].as_u64().unwrap()).sum();
            let groups = lib.candidates(id, ANY_BRAND).unwrap();
            println!("{id:13} {:4} brands {sets:4} sendable sets {:4} test codes", brands.len(), groups.len());
        }
        let led = lib.candidates("led_lighting", ANY_BRAND).unwrap();
        for g in led.iter().take(5) {
            let sets = g["sets"].as_array().unwrap();
            println!("  {:12} {:3} sets, first {} (check {})", g["button"], sets.len(), sets[0]["id"], sets[0]["check"]);
        }
    }
}
