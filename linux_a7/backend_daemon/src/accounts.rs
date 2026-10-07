/*
 * accounts.rs -- vendor accounts the hub is signed in to (issue #74).
 *
 * A vendor account (EZVIZ, Roborock...) is signed in to ONCE: the next
 * device from the same account is added with a tap ("Use
 * me@example.com"), and all of an account's devices share its session --
 * renewed once, for all of them. Settings > Accounts lists them; "Sign
 * out" forgets one (its devices then need "Pair again").
 *
 * WHAT'S KEPT: the vendor's session (tokens, keys), as the vendor's
 * adapter hands it over -- NEVER the password (the wizard forgets it
 * right after the login step). An account is identified as
 * "<adapter>:<email>", e.g. "ezviz:me@example.com"; a device made from it
 * has that in its config as "account".
 *
 * WHERE: in the secrets file (secrets.rs: only hubd can read it, never
 * logged, never sent to a client or the cloud), under the reserved entry
 * ACCOUNTS -- not a valid device id (shadow::valid_name has no "@"), so it
 * can never clash with a device's secrets. Each account is one value
 * there: {"vendor", "account", "session"} as JSON.
 */
use serde::{Deserialize, Serialize};

use crate::secrets::{Secret, Secrets};

const ACCOUNTS: &str = "@accounts";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Account {
    /* The adapter it's for ("ezviz", "roborock"). */
    pub vendor: String,
    /* What the person signs in with (an email, a phone number). */
    pub account: String,
    /* The vendor's session, as its adapter stores it (JSON). */
    pub session: String,
}

impl Account {
    pub fn id(&self) -> String {
        id(&self.vendor, &self.account)
    }
}

pub fn id(vendor: &str, account: &str) -> String {
    format!("{vendor}:{account}")
}

/* All saved accounts, optionally only one vendor's. */
pub fn list(secrets: &Secrets, vendor: Option<&str>) -> Vec<Account> {
    secrets
        .get(ACCOUNTS)
        .values()
        .filter_map(|s| serde_json::from_str::<Account>(s.expose()).ok())
        .filter(|a| vendor.is_none_or(|v| a.vendor == v))
        .collect()
}

pub fn get(secrets: &Secrets, id: &str) -> Option<Account> {
    secrets
        .get(ACCOUNTS)
        .get(id)
        .and_then(|s| serde_json::from_str(s.expose()).ok())
}

/* Saves (or replaces) an account -- a new login, or a renewed session. */
pub fn save(secrets: &Secrets, account: &Account) {
    let json = serde_json::to_string(account).expect("an account is always valid JSON");
    secrets.set(ACCOUNTS, [(account.id(), Secret::new(json))].into());
}

/* Just the session, renewed by a device task. Nothing if the account was
 * signed out meanwhile (the renewal mustn't bring it back). */
pub fn update_session(secrets: &Secrets, id: &str, session: String) {
    if let Some(mut account) = get(secrets, id) {
        account.session = session;
        save(secrets, &account);
    }
}

/* "Sign out": forgets the account's session. */
pub fn remove(secrets: &Secrets, id: &str) -> bool {
    secrets.remove_one(ACCOUNTS, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    #[test]
    fn accounts_are_saved_listed_and_removed() {
        let secrets = Secrets::new(Default::default(), watch::channel(Vec::new()).0);
        let ezviz = Account { vendor: "ezviz".into(), account: "me@example.com".into(), session: "{\"s\":1}".into() };
        save(&secrets, &ezviz);
        save(&secrets, &Account { vendor: "roborock".into(), account: "me@example.com".into(), session: "{}".into() });
        assert_eq!(list(&secrets, Some("ezviz")), vec![ezviz.clone()]);
        assert_eq!(list(&secrets, None).len(), 2);
        update_session(&secrets, "ezviz:me@example.com", "{\"s\":2}".into());
        assert_eq!(get(&secrets, "ezviz:me@example.com").unwrap().session, "{\"s\":2}");
        assert!(remove(&secrets, "ezviz:me@example.com"));
        /* A late renewal doesn't bring a signed-out account back. */
        update_session(&secrets, "ezviz:me@example.com", "{\"s\":3}".into());
        assert!(get(&secrets, "ezviz:me@example.com").is_none());
        assert_eq!(list(&secrets, None).len(), 1);
    }
}
