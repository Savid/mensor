//! Who may ask a host: whoever holds its token, or a token made from it for
//! one mind, or, for a host with no token, whoever asks from its own
//! machine.
//!
//! A token for a mind is its root name, a `.`, and the HMAC-SHA256 of that
//! name keyed with the host's token, in lowercase hex. It reaches that mind
//! and its children, whose names start with the root and a `.`: the minds a
//! hand of it may clone into. A host keeps no record of the tokens it has
//! made: it checks one by making it again, so a new host token ends every
//! token made from the old one.

use std::fmt::{self, Write};

use hmac::{Hmac, Mac};
use mensor_protocol::Name;
use sha2::{Digest, Sha256};

/// A host's token: the secret every request must carry, as a bearer token
/// or as the key of one made for a mind. It is never empty, and it shows as
/// `..` when debugged.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// `token`, unless it is empty.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        (!token.is_empty()).then_some(Self(token))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Whether a request's `Authorization` header carries `token` as a bearer
/// token. The two are compared as SHA-256 digests, in a time that depends
/// on neither what they hold nor how long the token is.
pub fn authorized(token: &Token, authorization: Option<&str>) -> bool {
    let given = authorization.and_then(|value| value.strip_prefix("Bearer "));
    given.is_some_and(|given| {
        let (given, token) = (Sha256::digest(given), Sha256::digest(&token.0));
        given
            .iter()
            .zip(token.iter())
            .fold(0, |differs, (a, b)| differs | (a ^ b))
            == 0
    })
}

/// What a request reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Every mind: the request holds the host's token, or the host has none.
    Host,
    /// The mind of this root name and its children, and no other.
    Root(Name),
}

impl Access {
    /// What a request with `authorization`, its `Authorization` header,
    /// reaches on a host whose token is `token`: nothing, unless it carries
    /// that token or one made from it as a bearer token.
    pub fn of(token: &Token, authorization: Option<&str>) -> Option<Self> {
        if authorized(token, authorization) {
            return Some(Self::Host);
        }
        let given = authorization?.strip_prefix("Bearer ")?;
        let (root, mac) = given.split_once('.')?;
        let root = Name::new(root).ok().filter(Name::is_root)?;
        let mac = unhex(mac)?;
        let mut expected = keyed(token);
        expected.update(root.as_str().as_bytes());
        expected.verify_slice(&mac).ok()?;
        Some(Self::Root(root))
    }

    /// Whether the mind `name` is within reach.
    pub fn reaches(&self, name: &Name) -> bool {
        match self {
            Self::Host => true,
            Self::Root(root) => name.root() == root.as_str(),
        }
    }
}

/// Why a token could not be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0} is not a mind that starts from nothing: a token is made for one, and reaches its children too"
)]
pub struct NotRoot(pub Name);

/// A token that reaches the mind `root` and its children, made from the
/// host's token. A hand given it in place of the host's joins only those
/// minds, and a client given it asks only about them.
pub fn mind_token(host: &Token, root: &Name) -> Result<String, NotRoot> {
    if !root.is_root() {
        return Err(NotRoot(root.clone()));
    }
    let mut mac = keyed(host);
    mac.update(root.as_str().as_bytes());
    let mut token = format!("{root}.");
    for byte in mac.finalize().into_bytes() {
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

fn keyed(host: &Token) -> Hmac<Sha256> {
    Hmac::new_from_slice(host.0.as_bytes()).expect("HMAC takes a key of any length")
}

/// The bytes that lowercase hex spells, if it spells a whole number of them.
fn unhex(hex: &str) -> Option<Vec<u8>> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let hex = hex.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

/// Whether a request comes from this machine, for a host with no token,
/// which trusts every process on it.
///
/// `host` is the request's `Host`, which must name this machine:
/// `localhost` or a loopback address, with or without a port. That stops a
/// page whose own name an attacker points at this machine. `origin` is the
/// `Origin` a browser sends with a request a page makes, and when there is
/// one it must name this machine too. That stops a page that asks this
/// machine directly, which a browser lets any page do, a WebSocket
/// included. A hand or a client outside a browser sends no `Origin`. Both
/// stop browsers only: any process on the machine may say what it likes.
pub fn local(host: Option<&str>, origin: Option<&str>) -> bool {
    let from_here = |origin: &str| {
        origin
            .split_once("://")
            .is_some_and(|(_, authority)| loopback(authority))
    };
    host.is_some_and(loopback) && origin.is_none_or(from_here)
}

/// Whether `authority`, a host and perhaps a port, names this machine.
fn loopback(authority: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    let v6 =
        |ip: Ipv6Addr| ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback());
    match authority.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => return ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => return v6(ip),
        Err(_) => {}
    }
    let port = |port: &str| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit());
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((address, rest)) = bracketed.split_once(']') else {
            return false;
        };
        let rest_is_a_port = rest.is_empty() || rest.strip_prefix(':').is_some_and(port);
        return rest_is_a_port && address.parse::<Ipv6Addr>().is_ok_and(v6);
    }
    let host = match authority.rsplit_once(':') {
        Some((host, after)) if port(after) => host,
        Some(_) => return false,
        None => authority,
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(token: &str) -> Token {
        Token::new(token).unwrap()
    }

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    #[test]
    fn a_token_is_never_empty_and_never_shown() {
        assert_eq!(Token::new(""), None);
        let shown = format!("{:?}", token("s3cret"));
        assert_eq!(shown, "Token(..)");
    }

    #[test]
    fn only_the_token_itself_is_authorized() {
        let host = token("s3cret");
        assert!(authorized(&host, Some("Bearer s3cret")));
        for other in [
            None,
            Some("s3cret"),
            Some("Bearer s3cre"),
            Some("Bearer s3crets"),
            Some("Bearer S3cret"),
            Some("Bearer "),
            Some("bearer s3cret"),
        ] {
            assert!(!authorized(&host, other), "{other:?}");
        }
    }

    #[test]
    fn the_hosts_token_reaches_every_mind() {
        let access = Access::of(&token("s3cret"), Some(&bearer("s3cret"))).unwrap();
        assert_eq!(access, Access::Host);
        assert!(access.reaches(&name("anything.at.all")));
    }

    #[test]
    fn a_minds_token_reaches_it_and_its_children_only() {
        let made = mind_token(&token("s3cret"), &name("m")).unwrap();
        assert!(made.starts_with("m."));
        assert_eq!(made.len(), "m.".len() + 64);
        let access = Access::of(&token("s3cret"), Some(&bearer(&made))).unwrap();
        assert_eq!(access, Access::Root(name("m")));
        assert!(access.reaches(&name("m")));
        assert!(access.reaches(&name("m.d3342iuj")));
        assert!(access.reaches(&name("m.d3342iuj.ckj6ldqi")));
        assert!(!access.reaches(&name("mm")));
        assert!(!access.reaches(&name("n")));
        assert!(!access.reaches(&name("n.d3342iuj")));
    }

    #[test]
    fn a_token_made_from_another_host_token_or_changed_reaches_nothing() {
        let host = token("s3cret");
        let made = mind_token(&host, &name("m")).unwrap();
        assert_eq!(Access::of(&token("other"), Some(&bearer(&made))), None);

        let as_another = made.replacen("m.", "n.", 1);
        assert_eq!(Access::of(&host, Some(&bearer(&as_another))), None);
        let mut flipped = made.clone();
        let last = if flipped.ends_with('0') { "1" } else { "0" };
        flipped.replace_range(flipped.len() - 1.., last);
        assert_eq!(Access::of(&host, Some(&bearer(&flipped))), None);

        for given in [
            made.to_uppercase(),
            format!("{made}0"),
            "m".into(),
            "m.".into(),
            "m.zz".into(),
            ".".into(),
            String::new(),
        ] {
            assert_eq!(Access::of(&host, Some(&bearer(&given))), None, "{given}");
        }
        assert_eq!(Access::of(&host, Some(&made)), None);
        assert_eq!(Access::of(&host, None), None);
    }

    #[test]
    fn a_token_is_made_only_for_a_mind_that_starts_from_nothing() {
        let host = token("s3cret");
        assert!(mind_token(&host, &name("m.d3342iuj")).is_err());
        let long = name(&"x".repeat(Name::ROOT_MAX_LEN + 1));
        assert!(mind_token(&host, &long).is_err());
    }

    #[test]
    fn a_request_addressed_to_this_machine_is_local() {
        for host in [
            "localhost",
            "LocalHost:7878",
            "127.0.0.1",
            "127.0.0.1:7878",
            "127.8.9.10:80",
            "[::1]",
            "[::1]:7878",
            "::1",
            "[::ffff:127.0.0.1]:7878",
        ] {
            assert!(local(Some(host), None), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:7878",
            "localhost.evil.example",
            "localhost:",
            "localhost:x",
            "10.0.0.1:7878",
            "[::2]:7878",
            "[::ffff:10.0.0.1]:7878",
            "[::1]x",
            "[::1",
            "",
        ] {
            assert!(!local(Some(host), None), "{host}");
        }
        assert!(!local(None, None));
    }

    #[test]
    fn a_page_from_elsewhere_is_not_local_though_it_asks_this_machine() {
        let here = Some("127.0.0.1:7878");
        for origin in [
            "http://localhost:3000",
            "http://127.0.0.1:7878",
            "https://[::1]",
        ] {
            assert!(local(here, Some(origin)), "{origin}");
        }
        for origin in [
            "https://evil.example",
            "http://evil.example:7878",
            "null",
            "",
        ] {
            assert!(!local(here, Some(origin)), "{origin}");
        }
    }
}
