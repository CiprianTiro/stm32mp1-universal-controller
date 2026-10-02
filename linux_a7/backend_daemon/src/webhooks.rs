/*
 * webhooks.rs -- devices that CALL the hub (issue #72).
 *
 * Many devices can call a web address when something happens: a Shelly's
 * "actions" (switched on, button pressed), Tasmota rules, ESPHome, DIY
 * buttons. Every device can get its own secret address:
 *
 *     http://<hub>:8780/hook/<token>
 *
 * The token (32 random characters) is the device's secret (secrets.rs,
 * "webhook_token"): removed with the device, never shown to clients other
 * than through webhook_url. A call tells that device's task "read your
 * state now" (adapters::Registry::refresh): a Shelly switched with its
 * button shows the change at once instead of at its next poll.
 *
 * Plain HTTP on the home network only (firewall: not the setup hotspot),
 * like the MQTT broker's port 1883 and for the same reason: devices can't
 * check the hub's self-signed certificate. What protects it: the token
 * can't be guessed, a call only makes the hub LOOK at the device (it
 * changes nothing), and each device's address takes at most MAX_CALLS per
 * WINDOW.
 */
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use base64::Engine;
use ring::rand::SecureRandom;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::control::Control;
use crate::secrets::Secret;
use crate::state::DeviceId;

pub const PORT: u16 = 8780;
const SECRET_NAME: &str = "webhook_token";
const MAX_CALLS: usize = 10;
const WINDOW: Duration = Duration::from_secs(10);

pub struct Webhooks {
    /* token -> device. */
    tokens: Mutex<HashMap<String, DeviceId>>,
    /* Each device's recent calls (the rate limit). */
    recent: Mutex<HashMap<DeviceId, VecDeque<Instant>>>,
    control: Control,
}

/* What a call did. */
#[derive(Debug, PartialEq)]
pub enum Hit {
    /* Its task was told to look. */
    Refreshed(DeviceId),
    /* A device that's told by itself anyway (nothing to refresh). */
    Noted(DeviceId),
    Unknown,
    TooMany,
}

impl Webhooks {
    pub fn new(control: Control) -> Self {
        let tokens = control
            .secrets()
            .all_named(SECRET_NAME)
            .into_iter()
            .map(|(device, token)| (token.expose().to_string(), device))
            .collect();
        Webhooks {
            tokens: Mutex::new(tokens),
            recent: Mutex::new(HashMap::new()),
            control,
        }
    }

    /* The device's address (made the first time). */
    pub async fn url_for(&self, id: &str) -> Result<String, String> {
        if self.control.get(id).await?.is_none() {
            return Err(format!("there's no device {id:?}"));
        }
        let existing = self.control.secrets().get(id).get(SECRET_NAME).cloned();
        let token = match existing {
            Some(token) => token.expose().to_string(),
            None => {
                let mut bytes = [0u8; 24];
                ring::rand::SystemRandom::new()
                    .fill(&mut bytes)
                    .map_err(|_| "no random numbers".to_string())?;
                let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
                self.control
                    .secrets()
                    .set(id, BTreeMap::from([(SECRET_NAME.to_string(), Secret::new(token.clone()))]));
                token
            }
        };
        self.tokens.lock().unwrap().insert(token.clone(), id.to_string());
        Ok(format!("http://{}:{PORT}/hook/{token}", crate::network::lan_address()))
    }

    /* One call. */
    pub async fn hit(&self, token: &str) -> Hit {
        let Some(id) = self.tokens.lock().unwrap().get(token).cloned() else {
            return Hit::Unknown;
        };
        /* Removed since (its secrets went with it): forget the token too. */
        if !matches!(self.control.get(&id).await, Ok(Some(_))) {
            self.tokens.lock().unwrap().remove(token);
            return Hit::Unknown;
        }
        {
            let mut recent = self.recent.lock().unwrap();
            let calls = recent.entry(id.clone()).or_default();
            let now = Instant::now();
            calls.retain(|t| now.duration_since(*t) < WINDOW);
            if calls.len() >= MAX_CALLS {
                return Hit::TooMany;
            }
            calls.push_back(now);
        }
        if self.control.refresh(&id) {
            Hit::Refreshed(id)
        } else {
            Hit::Noted(id)
        }
    }
}

/* GET or POST /hook/<token>: 204 when it was a device's, 404 when not
 * (the same answer for "no such token" and "removed device"), 429 when it
 * calls too often. The body isn't read. */
async fn call(State(hooks): State<Arc<Webhooks>>, Path(token): Path<String>) -> StatusCode {
    match hooks.hit(&token).await {
        Hit::Refreshed(id) | Hit::Noted(id) => {
            println!("webhooks: {id} called");
            StatusCode::NO_CONTENT
        }
        Hit::Unknown => StatusCode::NOT_FOUND,
        Hit::TooMany => StatusCode::TOO_MANY_REQUESTS,
    }
}

/* Runs for the daemon's lifetime (spawned from main). */
pub async fn run(hooks: Arc<Webhooks>) {
    let router = Router::new()
        .route("/hook/:token", get(call).post(call))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .with_state(hooks);
    match tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(listener) => {
            println!("webhooks: listening on port {PORT}");
            if let Err(e) = axum::serve(listener, router).await {
                println!("webhooks: stopped: {e}");
            }
        }
        Err(e) => println!("webhooks: can't listen on port {PORT}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_hub::TestHub;
    use crate::device::Device;
    use serde_json::json;

    async fn hub_with_one_device() -> TestHub {
        let d: Device = serde_json::from_value(json!({
            "id": "lamp", "name": "Lamp", "source": "virtual", "capabilities": {"switch": {"on": false}}
        }))
        .unwrap();
        TestHub::start(d, Box::new(crate::adapters::wled::Wled)).await
    }

    #[tokio::test]
    async fn tokens_are_per_device_secret_and_rate_limited() {
        let hub = hub_with_one_device().await;
        let hooks = Webhooks::new(hub.control.clone());
        let url = hooks.url_for("lamp").await.unwrap();
        let token = url.rsplit('/').next().unwrap().to_string();
        assert_eq!(token.len(), 32);
        /* The same address again, not a new one. */
        assert_eq!(hooks.url_for("lamp").await.unwrap(), url);
        assert!(hooks.url_for("ghost").await.is_err());
        /* Kept as the device's secret: a restart finds it again. */
        assert_eq!(Webhooks::new(hub.control.clone()).hit(&token).await, Hit::Noted("lamp".into()));

        assert_eq!(hooks.hit("not-a-token").await, Hit::Unknown);
        for _ in 0..MAX_CALLS {
            assert_eq!(hooks.hit(&token).await, Hit::Noted("lamp".into()), "a virtual device has nothing to refresh");
        }
        assert_eq!(hooks.hit(&token).await, Hit::TooMany);

        /* Removed: its address is dead. */
        hub.control.remove("lamp").await.unwrap();
        assert_eq!(hooks.hit(&token).await, Hit::Unknown);
    }
}
