pub mod auth;
pub mod cache;
pub mod gateway;
pub mod login;
pub mod media_signaling;
pub mod models;
pub mod permissions;
pub mod rest;
mod zlib_stream;

const DESKTOP_UA: &str = concat!(
    "Suncord/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Alloyage/Suncord)"
);

// Keep HTTP authentication and gateway identification consistent.
pub fn client_properties() -> serde_json::Value {
    serde_json::json!({
        "os": if cfg!(target_os = "windows") { "Windows" } else { "Linux" },
        "browser": "Suncord",
        "device": "Suncord",
        "system_locale": "en-US",
        "browser_user_agent": DESKTOP_UA,
        "browser_version": env!("CARGO_PKG_VERSION")
    })
}

pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(DESKTOP_UA)
        .build()
}

pub fn snowflake(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}
