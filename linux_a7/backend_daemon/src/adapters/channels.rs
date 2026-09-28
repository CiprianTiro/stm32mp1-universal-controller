/*
 * channels.rs -- searching and paging a TV's channel list (issue #44),
 * the same for every TV adapter.
 *
 * A TV can have thousands of channels (satellite: 5000+). Sending them all
 * to a screen at once is slow to draw on the hub and heavy for a phone, and
 * scrolling through them is no way to find one. So a TV adapter keeps the
 * whole list itself, and the `channels` action (device::check_action)
 * returns ONE PAGE of the channels that match a search:
 *
 *   channels {"query": "pro", "offset": 0, "limit": 100}
 *     -> {"channels": [{"id", "number", "name"}, ...], "total": 7}
 *
 * `total` is how many match (to offer "show more"). Every argument is
 * optional: no query = all channels, in the TV's own order.
 *
 * THE SEARCH: a query of digits finds channels whose NUMBER starts with it
 * ("7": 7, 70, 712 -- the exact number first); any query also finds a
 * NAME containing it. Case and accents don't matter: "stiri" finds
 * "Știri", "antena" finds "ANTENA 1" -- people type on a keyboard without
 * diacritics.
 *
 * Pure (no I/O), unit-tested below.
 */
use serde_json::{json, Value};

use crate::device::Channel;

/* One page, at most. */
pub const MAX_LIMIT: u64 = 500;
pub const DEFAULT_LIMIT: u64 = 100;

/* What the `channels` action asks for (its checked arguments). */
pub struct Query {
    pub text: String,
    pub offset: usize,
    pub limit: usize,
}

impl Query {
    /* From the action's arguments (device::check_action checked them). */
    pub fn from_args(args: &Value) -> Query {
        Query {
            text: args["query"].as_str().unwrap_or_default().trim().to_string(),
            offset: args["offset"].as_u64().unwrap_or(0) as usize,
            limit: args["limit"].as_u64().unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize,
        }
    }

    /* The first page of the whole list: what opening the list asks for
     * (a TV adapter fetches the list fresh then, and uses what it has for
     * searches and further pages). */
    pub fn is_fresh_look(&self) -> bool {
        self.text.is_empty() && self.offset == 0
    }
}

/* The matching channels' page, as the action's result. */
pub fn page(all: &[Channel], query: &Query) -> Value {
    let found = search(all, &query.text);
    let total = found.len();
    let page: Vec<&Channel> = found.into_iter().skip(query.offset).take(query.limit).collect();
    json!({ "channels": page, "total": total })
}

fn search<'a>(all: &'a [Channel], text: &str) -> Vec<&'a Channel> {
    if text.is_empty() {
        return all.iter().collect();
    }
    let wanted = fold(text);
    let digits = text.bytes().all(|b| b.is_ascii_digit());
    let mut exact = Vec::new();
    let mut others = Vec::new();
    for channel in all {
        if digits && channel.number == text {
            exact.push(channel);
        } else if (digits && channel.number.starts_with(text)) || fold(&channel.name).contains(&wanted) {
            others.push(channel);
        }
    }
    exact.extend(others);
    exact
}

/* Lower case, without accents: "Știri" -> "stiri". A table for the Latin
 * letters European channel names use (Latin-1, Latin Extended-A, Romanian
 * ș ț in both forms) -- no Unicode library needed on the hub for this. */
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
            'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
            'ł' | 'ĺ' | 'ļ' | 'ľ' => 'l',
            'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
            'ŕ' | 'ŗ' | 'ř' => 'r',
            'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => 's',
            'ţ' | 'ť' | 'ț' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
            'ý' | 'ÿ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels() -> Vec<Channel> {
        [("1", "TVR 1"), ("2", "Știri TV"), ("7", "ANTENA 1"), ("70", "Pro Cinema"), ("712", "Sport 7"), ("8", "Pro TV")]
            .iter()
            .map(|&(number, name)| Channel {
                id: format!("ch-{number}"),
                number: number.into(),
                name: name.into(),
            })
            .collect()
    }

    fn numbers(result: &Value) -> Vec<String> {
        result["channels"].as_array().unwrap().iter().map(|c| c["number"].as_str().unwrap().to_string()).collect()
    }

    fn find(text: &str) -> Vec<String> {
        numbers(&page(&channels(), &Query::from_args(&json!({ "query": text }))))
    }

    #[test]
    fn names_ignore_case_and_accents() {
        assert_eq!(find("stiri"), ["2"]);
        assert_eq!(find("antena"), ["7"]);
        assert_eq!(find("PRO"), ["70", "8"]);
        assert_eq!(find("  pro  "), ["70", "8"], "spaces around are ignored");
        assert!(find("nothing").is_empty());
    }

    #[test]
    fn digits_find_numbers_exact_first_and_names_too() {
        /* 7 exactly, then numbers starting with 7, then "Sport 7"'s name
         * (712 already counted by its number). */
        assert_eq!(find("7"), ["7", "70", "712"]);
        assert_eq!(find("1"), ["1", "7"], "TVR 1 by number, ANTENA 1 by name");
    }

    #[test]
    fn pages_and_total() {
        let all = channels();
        let first = page(&all, &Query::from_args(&json!({ "limit": 4 })));
        assert_eq!(numbers(&first), ["1", "2", "7", "70"]);
        assert_eq!(first["total"], 6);
        let rest = page(&all, &Query::from_args(&json!({ "offset": 4, "limit": 4 })));
        assert_eq!(numbers(&rest), ["712", "8"]);
        assert_eq!(page(&all, &Query::from_args(&json!({ "offset": 99 })))["channels"], json!([]));
        /* Limits are capped. */
        assert_eq!(Query::from_args(&json!({ "limit": 100000 })).limit, MAX_LIMIT as usize);
        assert!(Query::from_args(&json!({})).is_fresh_look());
        assert!(!Query::from_args(&json!({ "query": "x" })).is_fresh_look());
    }
}
