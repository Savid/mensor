//! Where a database is, and how a connection to it is made.
//!
//! A database is named by a URL, `postgres://` or `postgresql://`, with the
//! options libpq's URLs take. These decide how it is reached:
//!
//! - `sslmode`: `disable` is plain. Every other mode libpq knows (`allow`,
//!   `prefer`, `require`, `verify-ca`, `verify-full`) is TLS checked as
//!   libpq's `verify-full` checks it: the server's certificate against the
//!   roots, and its name against the host's. A connection never falls back
//!   to plain on its own. A host that is a Unix socket is reached plain, as
//!   libpq reaches it.
//! - `sslrootcert`: a file of PEM certificates the server's must be signed
//!   by, in place of the system's roots. `system` names the system's.
//! - `channel_binding=require` is refused: binding proves the server where
//!   its certificate is not checked, and here it always is.
//!
//! TLS is 1.3, with ring, as mensor's is elsewhere.
//!
//! A connection notices its server gone within about a minute: keepalives
//! after 30 s idle, every 10 s, given up after 3, and 60 s for what was sent
//! and not acknowledged. A URL that sets one of these keeps its own,
//! `tcp_user_timeout` in milliseconds as libpq reads it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use mensor_mind::StoreError;
use percent_encoding::percent_decode_str;
use postgres::config::{Host, SslMode};
use postgres::{Client, Config, NoTls};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use crate::tls::Tls;

/// The `sslmode`s libpq knows.
const MODES: [&str; 6] = [
    "disable",
    "allow",
    "prefer",
    "require",
    "verify-ca",
    "verify-full",
];

/// A database, and how to reach it.
pub(crate) struct Target {
    config: Config,
    /// How the server is checked, or `None` to speak plain.
    tls: Option<Tls>,
}

impl Target {
    /// The database `url` names. Its password stays in the config, which
    /// never shows it: no error here quotes the URL.
    pub(crate) fn parse(url: &str) -> Result<Self, StoreError> {
        let options = Options::of(url)?;
        let mut config = Config::from_str(&options.rest)
            .map_err(|error| StoreError::Backend(format!("the Postgres URL is wrong: {error}")))?;
        options.limit(&mut config);
        let plain = options.mode.as_deref() == Some("disable") || sockets(&config);
        let tls = match (plain, options.root) {
            (true, Some(_)) if !sockets(&config) => {
                return Err(StoreError::Backend(
                    "the Postgres URL names an sslrootcert, but its sslmode is disable".into(),
                ));
            }
            (true, _) => {
                config.ssl_mode(SslMode::Disable);
                None
            }
            (false, root) => {
                config.ssl_mode(SslMode::Require);
                let tls = Tls::new(roots(root.as_ref())?);
                Some(tls.map_err(|error| StoreError::Backend(format!("TLS: {error}")))?)
            }
        };
        Ok(Self { config, tls })
    }

    /// A new connection.
    pub(crate) fn connect(&self) -> Result<Client, postgres::Error> {
        match &self.tls {
            Some(tls) => self.config.connect(tls.clone()),
            None => self.config.connect(NoTls),
        }
    }
}

/// Where a server's certificate is checked from.
enum Root {
    System,
    File(PathBuf),
}

/// What of a URL this store reads itself: the client knows only some of
/// libpq's `sslmode`s, no `sslrootcert`, and reads `tcp_user_timeout` in
/// seconds where libpq reads milliseconds.
struct Options {
    /// The URL without them.
    rest: String,
    mode: Option<String>,
    root: Option<Root>,
    unacknowledged: Option<Duration>,
    /// The options the URL names.
    named: Vec<String>,
}

impl Options {
    fn of(url: &str) -> Result<Self, StoreError> {
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(StoreError::Backend(
                "a Postgres database is named by a URL that starts postgres:// or postgresql://"
                    .into(),
            ));
        }
        let (base, query) = url.split_once('?').unwrap_or((url, ""));
        let mut options = Self {
            rest: base.to_owned(),
            mode: None,
            root: None,
            unacknowledged: None,
            named: Vec::new(),
        };
        let mut kept = Vec::new();
        for option in query.split('&').filter(|option| !option.is_empty()) {
            let (key, value) = option.split_once('=').unwrap_or((option, ""));
            let value = decode(value)?;
            options.named.push(key.to_owned());
            match key {
                "sslmode" if MODES.contains(&value.as_ref()) => {
                    options.mode = Some(value.into_owned());
                }
                "sslmode" => {
                    return Err(StoreError::Backend(format!(
                        "the Postgres URL's sslmode is none of libpq's: {}",
                        MODES.join(", ")
                    )));
                }
                "tcp_user_timeout" => {
                    let milliseconds = value.parse().map_err(|_| {
                        StoreError::Backend(
                            "the Postgres URL's tcp_user_timeout is not milliseconds".into(),
                        )
                    })?;
                    options.unacknowledged = Some(Duration::from_millis(milliseconds));
                }
                "sslrootcert" if value == "system" => options.root = Some(Root::System),
                "sslrootcert" => options.root = Some(Root::File(PathBuf::from(value.as_ref()))),
                "channel_binding" if value == "require" => {
                    return Err(StoreError::Backend(
                        "the Postgres URL asks for channel_binding=require, which this store \
                         does not do: it always checks the server's certificate, which proves \
                         the server"
                            .into(),
                    ));
                }
                _ => kept.push(option),
            }
        }
        if !kept.is_empty() {
            options.rest = format!("{base}?{}", kept.join("&"));
        }
        Ok(options)
    }

    /// Sets the limits a URL does not set itself.
    fn limit(&self, config: &mut Config) {
        let unset = |key: &str| !self.named.iter().any(|named| named == key);
        if unset("keepalives_idle") {
            config.keepalives_idle(Duration::from_secs(30));
        }
        if unset("keepalives_interval") {
            config.keepalives_interval(Duration::from_secs(10));
        }
        if unset("keepalives_retries") {
            config.keepalives_retries(3);
        }
        let unacknowledged = self.unacknowledged.unwrap_or(Duration::from_mins(1));
        if !unacknowledged.is_zero() {
            config.tcp_user_timeout(unacknowledged);
        }
    }
}

/// A URL option's value, its `%XX` escapes decoded.
fn decode(value: &str) -> Result<Cow<'_, str>, StoreError> {
    percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| StoreError::Backend("the Postgres URL escapes something not UTF-8".into()))
}

/// Whether every host the config names is a Unix socket.
fn sockets(config: &Config) -> bool {
    let hosts = config.get_hosts();
    !hosts.is_empty()
        && hosts.iter().all(|host| match host {
            Host::Tcp(_) => false,
            #[cfg(unix)]
            Host::Unix(_) => true,
        })
}

/// The roots a server's certificate must be signed by: those in the file
/// `root` names, or else the system's.
fn roots(root: Option<&Root>) -> Result<Arc<RootCertStore>, StoreError> {
    let mut roots = RootCertStore::empty();
    let certificates: Vec<CertificateDer<'static>> = match root {
        Some(Root::File(path)) => read(path)?,
        Some(Root::System) | None => rustls_native_certs::load_native_certs().certs,
    };
    roots.add_parsable_certificates(certificates);
    if roots.is_empty() {
        let source = match root {
            Some(Root::File(path)) => path.display().to_string(),
            Some(Root::System) | None => "the system".to_owned(),
        };
        return Err(StoreError::Backend(format!(
            "{source} has no root certificate to check a Postgres server's by"
        )));
    }
    Ok(Arc::new(roots))
}

fn read(path: &Path) -> Result<Vec<CertificateDer<'static>>, StoreError> {
    CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect)
        .map_err(|error| {
            StoreError::Backend(format!("{} could not be read: {error}", path.display()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaks_tls(url: &str) -> bool {
        Target::parse(url).unwrap().tls.is_some()
    }

    #[test]
    fn a_connection_is_tls_unless_its_url_says_disable() {
        assert!(speaks_tls("postgres://mensor@db.example/mensor"));
        for mode in ["allow", "prefer", "require", "verify-ca", "verify-full"] {
            let url = format!("postgresql://db.example/mensor?sslmode={mode}");
            let target = Target::parse(&url).unwrap();
            assert!(target.tls.is_some(), "{mode}");
            assert_eq!(target.config.get_ssl_mode(), SslMode::Require, "{mode}");
        }
        assert!(speaks_tls("postgres://db.example/m?sslrootcert=system"));
        assert!(!speaks_tls("postgres://db.example/mensor?sslmode=disable"));
        // A socket is reached plain, whatever the mode, as libpq reaches it.
        assert!(!speaks_tls(
            "postgres:///mensor?host=/run/postgresql&sslmode=verify-full"
        ));
        let refused = Target::parse("postgres://db.example/m?sslmode=sometimes").err();
        assert!(refused.unwrap().to_string().contains("verify-full"));
    }

    #[test]
    fn a_connection_gives_up_on_a_server_gone_within_a_minute_unless_its_url_says() {
        let target = Target::parse("postgres://db.example/m").unwrap();
        assert_eq!(target.config.get_keepalives_idle(), Duration::from_secs(30));
        assert_eq!(
            target.config.get_keepalives_interval(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(target.config.get_keepalives_retries(), Some(3));
        assert_eq!(
            target.config.get_tcp_user_timeout(),
            Some(&Duration::from_mins(1))
        );
        let own = Target::parse("postgres://db.example/m?keepalives_idle=5&tcp_user_timeout=7000");
        let own = own.unwrap();
        assert_eq!(own.config.get_keepalives_idle(), Duration::from_secs(5));
        assert_eq!(
            own.config.get_tcp_user_timeout(),
            Some(&Duration::from_secs(7))
        );
    }

    #[test]
    fn a_root_is_read_from_the_file_the_url_names() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty root.pem");
        std::fs::write(&empty, "").unwrap();
        let named = format!(
            "postgres://db.example/mensor?application_name=m&sslrootcert={}",
            empty.display().to_string().replace(' ', "%20")
        );
        let refused = Target::parse(&named).err().unwrap().to_string();
        assert!(refused.contains("empty root.pem has no root"), "{refused}");
        let missing = "postgres://db.example/m?sslrootcert=/no/such.pem";
        let refused = Target::parse(missing).err().unwrap().to_string();
        assert!(
            refused.contains("/no/such.pem could not be read"),
            "{refused}"
        );
        let plain = "postgres://db.example/m?sslmode=disable&sslrootcert=/no/such.pem";
        let refused = Target::parse(plain).err().unwrap().to_string();
        assert!(refused.contains("sslmode is disable"), "{refused}");
    }

    #[test]
    fn channel_binding_required_is_refused_and_says_why() {
        let refused = Target::parse("postgres://db.example/m?channel_binding=require").err();
        let refused = refused.unwrap().to_string();
        assert!(
            refused.contains("checks the server's certificate"),
            "{refused}"
        );
        assert!(speaks_tls("postgres://db.example/m?channel_binding=prefer"));
    }

    #[test]
    fn a_url_that_is_wrong_is_refused_without_its_password() {
        for url in [
            "host=db.example password=not-a-secret",
            "postgres://mensor:not-a-secret@db.example/m?nonsense=1",
            "postgres://mensor:not-a-secret@db.example/m?sslmode=sometimes",
            "postgres://mensor:not-a-secret@db.example/m?sslrootcert=%zz",
            "postgres://mensor:not-a-secret@db.example/m?sslrootcert=%FF",
        ] {
            let refused = Target::parse(url).err().unwrap().to_string();
            assert!(!refused.contains("not-a-secret"), "{refused}");
        }
    }
}
