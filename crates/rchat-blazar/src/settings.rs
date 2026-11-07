//! Application settings: module-owned typed contract with public defaults,
//! loaded from `public.toml` beside the checkout. Secrets never live here —
//! credentials come from the environment (`RCHAT_USER`/`RCHAT_PASSWORD`).

use crate::rc::RcSettings;

/// The whole settings file.
#[derive(Clone, Default)]
pub struct AppSettings {
    pub server: RcSettings,
}

impl AppSettings {
    /// Reads `public.toml` when present; a missing file or a missing
    /// `[server]` table falls back to the public defaults, while a malformed
    /// value is an error — wrong settings must not be silently ignored.
    pub fn load() -> Result<Self, String> {
        let raw = match std::fs::read_to_string("public.toml") {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("public.toml unreadable: {error}")),
        };
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        Self::load_from(&raw)
    }

    /// Loads from in-memory TOML; also the test seam.
    fn load_from(raw: &str) -> Result<Self, String> {
        let parsed: PublicToml = toml::from_str(raw).map_err(|error| error.to_string())?;
        match parsed.server.map(|server| server.url) {
            Some(url) => RcSettings::parse(&url)
                .map_err(|error| error.to_string())
                .map(|server| Self { server }),
            None => Ok(Self::default()),
        }
    }
}

#[derive(serde::Deserialize)]
struct PublicToml {
    server: Option<ServerTable>,
}

#[derive(serde::Deserialize)]
struct ServerTable {
    url: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_through() {
        let settings = AppSettings::default();
        assert_eq!(settings.server.addr(), ("127.0.0.1".to_owned(), 3000));
    }

    #[test]
    fn parses_server_url() {
        let settings =
            AppSettings::load_from("[server]\nurl = \"http://192.168.1.10:3000\"\n").unwrap();
        assert_eq!(settings.server.addr(), ("192.168.1.10".to_owned(), 3000));
        assert!(!settings.server.is_tls());
    }

    #[test]
    fn parses_https_with_default_port() {
        let settings =
            AppSettings::load_from("[server]\nurl = \"https://rocketchat.example.com\"\n").unwrap();
        assert!(settings.server.is_tls());
        assert_eq!(settings.server.addr().1, 443);
    }

    #[test]
    fn rejects_malformed_url() {
        assert!(AppSettings::load_from("[server]\nurl = \"ftp://x\"\n").is_err());
        assert!(AppSettings::load_from("[server]\nurl = \"http://\"\n").is_err());
    }
}
