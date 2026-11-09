//! Application settings: module-owned typed contract with public defaults,
//! loaded from `public.toml` beside the checkout. Secrets never live here —
//! credentials come from the environment (`RCHAT_USER`/`RCHAT_PASSWORD`).

use crate::rc::RcSettings;

/// The composer's Enter semantics (m14 P2, RC `sendOnEnter`):
/// `normal` sends on Enter and inserts on any modifier+Enter;
/// `alternative` inverts the roles; RC's `desktop` variant
/// resolves to `normal` here — the app is desktop-only
/// (DECL0002), so its form-factor check always lands on normal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SendOnEnter {
    #[default]
    Normal,
    Alternative,
}

/// The `[composer]` table.
#[derive(Clone, Copy, Debug, Default)]
pub struct ComposerSettings {
    pub send_on_enter: SendOnEnter,
}

/// The whole settings file.
#[derive(Clone, Default)]
pub struct AppSettings {
    pub server: RcSettings,
    pub composer: ComposerSettings,
}

impl AppSettings {
    /// Reads `public.toml` when present; a missing file or a missing
    /// `[server]` table falls back to the public defaults, while a malformed
    /// value is an error — wrong settings must not be silently ignored.
    /// `RCHAT_URL` overrides the file: a concrete deployment's address is
    /// instance-specific and stays out of the repository.
    pub fn load() -> Result<Self, String> {
        match std::env::var("RCHAT_URL") {
            Ok(url) => return Self::from_url(&url),
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => return Err(error.to_string()),
        }
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

    /// Parses a server URL into the settings.
    fn from_url(url: &str) -> Result<Self, String> {
        RcSettings::parse(url)
            .map_err(|error| error.to_string())
            .map(|server| Self {
                server,
                composer: ComposerSettings::default(),
            })
    }

    /// Loads from in-memory TOML; also the test seam.
    fn load_from(raw: &str) -> Result<Self, String> {
        let parsed: PublicToml = toml::from_str(raw).map_err(|error| error.to_string())?;
        let composer = match parsed.composer {
            Some(table) => ComposerSettings {
                send_on_enter: table
                    .send_on_enter
                    .map(SendOnEnter::from_config)
                    .transpose()?
                    .unwrap_or_default(),
            },
            None => ComposerSettings::default(),
        };
        match parsed.server.map(|server| server.url) {
            Some(url) => Ok(Self {
                server: RcSettings::parse(&url).map_err(|error| error.to_string())?,
                composer,
            }),
            None => Ok(Self {
                server: RcSettings::default(),
                composer,
            }),
        }
    }
}

impl SendOnEnter {
    /// RC's three-value preference: `desktop` resolves to `normal`
    /// on a desktop-only client (DECL0002).
    fn from_config(raw: String) -> Result<Self, String> {
        match raw.as_str() {
            "normal" | "desktop" => Ok(Self::Normal),
            "alternative" => Ok(Self::Alternative),
            other => Err(format!(
                "composer.send_on_enter: unknown value {other:?} (normal | alternative | desktop)"
            )),
        }
    }
}

#[derive(serde::Deserialize)]
struct PublicToml {
    server: Option<ServerTable>,
    composer: Option<ComposerTable>,
}

#[derive(serde::Deserialize)]
struct ComposerTable {
    send_on_enter: Option<String>,
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
    fn parses_override_url() {
        let settings = AppSettings::from_url("https://chat.example.com").unwrap();
        assert!(settings.server.is_tls());
        assert_eq!(settings.server.addr().1, 443);
    }

    #[test]
    fn rejects_malformed_url() {
        assert!(AppSettings::load_from("[server]\nurl = \"ftp://x\"\n").is_err());
        assert!(AppSettings::load_from("[server]\nurl = \"http://\"\n").is_err());
    }
    #[test]
    fn send_on_enter_config_parses_all_three_values() {
        let parse = |raw: &str| AppSettings::load_from(raw).unwrap().composer.send_on_enter;
        assert_eq!(
            parse("[composer]\nsend_on_enter = \"normal\""),
            SendOnEnter::Normal
        );
        // `desktop` resolves to `normal`: desktop-only client (DECL0002).
        assert_eq!(
            parse("[composer]\nsend_on_enter = \"desktop\""),
            SendOnEnter::Normal
        );
        assert_eq!(
            parse("[composer]\nsend_on_enter = \"alternative\""),
            SendOnEnter::Alternative
        );
        assert!(AppSettings::load_from("[composer]\nsend_on_enter = \"weird\"").is_err());
    }
}
