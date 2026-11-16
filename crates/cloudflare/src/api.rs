//! The API as this worker serves it, with no I/O: which request means
//! what, and who may ask. The bodies of its requests and answers are
//! [`mensor_api`]'s, the ones `mensor serve` uses.

use mensor_api::{Problem, Writing, authorized};
use mensor_mind::Item;
use mensor_protocol::Name;

/// What a request asks of one pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// `POST /v1/minds/{name}/prompt`
    Prompt(Name),
    /// `POST /v1/minds/{name}/interrupt`
    Interrupt(Name),
    /// `GET /v1/minds/{name}`
    Status(Name),
    /// `GET /v1/minds/{name}/history`
    History(Name),
    /// `GET /v1/minds/{name}/events`
    Events(Name),
}

impl Route {
    /// The pair asked about.
    pub(crate) fn name(&self) -> &Name {
        match self {
            Self::Prompt(name)
            | Self::Interrupt(name)
            | Self::Status(name)
            | Self::History(name)
            | Self::Events(name) => name,
        }
    }
}

/// Reads a request's method and path.
pub(crate) fn route(method: &str, path: &str) -> Result<Route, Problem> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (name, rest) = match parts[..] {
        ["v1", "minds"] => {
            return Err(Problem::new(
                501,
                "each pair is an object of its own, and none of them lists the others",
            ));
        }
        ["v1", "minds", name, ref rest @ ..] => (name, rest),
        _ => return Err(Problem::new(404, format!("nothing is served at {path}"))),
    };
    let name = Name::new(name).map_err(|error| Problem::new(404, error.to_string()))?;
    let (allowed, route) = match rest {
        [] => ("GET", Route::Status(name)),
        ["prompt"] => ("POST", Route::Prompt(name)),
        ["interrupt"] => ("POST", Route::Interrupt(name)),
        ["history"] => ("GET", Route::History(name)),
        ["events"] => ("GET", Route::Events(name)),
        _ => return Err(Problem::new(404, format!("nothing is served at {path}"))),
    };
    if method == allowed {
        Ok(route)
    } else {
        Err(Problem::new(405, format!("{path} takes {allowed}")))
    }
}

/// Whether a request may be served.
///
/// With a token, the request must carry it as a bearer token. Without one,
/// only a request addressed to this machine is served, which is how a
/// worker runs under local development and never how it is reached once
/// deployed.
pub(crate) fn permitted(token: Option<&str>, authorization: Option<&str>, host: &str) -> bool {
    match token {
        Some(token) => authorized(token, authorization),
        None => matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"),
    }
}

/// The `after` of a request's query, or 0.
pub(crate) fn after(query: Option<&str>) -> Result<u64, Problem> {
    let Some(value) = query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix("after="))
    else {
        return Ok(0);
    };
    value
        .parse()
        .map_err(|_| Problem::new(400, "after is the index of an item"))
}

/// One item as a server-sent event whose id is the item's index.
pub(crate) fn event(index: u64, item: &Item) -> Vec<u8> {
    let data = serde_json::to_string(item).unwrap_or_else(|_| "null".into());
    format!("id: {index}\ndata: {data}\n\n").into_bytes()
}

/// Text a model is writing, as a server-sent event with no id.
pub(crate) fn writing(at: u64, text: &str) -> Vec<u8> {
    let data = serde_json::to_string(&Writing { at, text }).unwrap_or_else(|_| "null".into());
    format!("event: {}\ndata: {data}\n\n", Writing::EVENT).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    #[test]
    fn each_path_is_one_question_about_one_pair() {
        assert_eq!(route("GET", "/v1/minds/a1"), Ok(Route::Status(name("a1"))));
        assert_eq!(
            route("POST", "/v1/minds/a1/prompt"),
            Ok(Route::Prompt(name("a1")))
        );
        assert_eq!(
            route("POST", "/v1/minds/a1/interrupt"),
            Ok(Route::Interrupt(name("a1")))
        );
        assert_eq!(
            route("GET", "/v1/minds/a1/history"),
            Ok(Route::History(name("a1")))
        );
        assert_eq!(
            route("GET", "/v1/minds/a1/events/"),
            Ok(Route::Events(name("a1")))
        );
        assert_eq!(route("GET", "/v1/minds/a1").unwrap().name(), &name("a1"));
    }

    #[test]
    fn what_is_not_served_says_why() {
        let status = |method, path| route(method, path).unwrap_err().status;
        assert_eq!(status("GET", "/v1/minds"), 501);
        assert_eq!(status("GET", "/"), 404);
        assert_eq!(status("GET", "/v1/minds/a1/files"), 404);
        assert_eq!(status("GET", "/v1/minds/not a name"), 404);
        assert_eq!(status("GET", "/v1/minds/a1/prompt"), 405);
        assert_eq!(status("POST", "/v1/minds/a1"), 405);
    }

    #[test]
    fn a_token_is_required_wherever_one_is_set() {
        assert!(permitted(
            Some("s3cret"),
            Some("Bearer s3cret"),
            "example.com"
        ));
        assert!(!permitted(
            Some("s3cret"),
            Some("Bearer s3cres"),
            "localhost"
        ));
        assert!(!permitted(Some("s3cret"), Some("s3cret"), "localhost"));
        assert!(!permitted(Some("s3cret"), None, "localhost"));
    }

    #[test]
    fn without_a_token_only_this_machine_is_served() {
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            assert!(permitted(None, None, host), "{host}");
        }
        assert!(!permitted(None, None, "mensor.example.workers.dev"));
        assert!(!permitted(None, Some("Bearer anything"), "example.com"));
    }

    #[test]
    fn a_cursor_is_read_from_the_query() {
        assert_eq!(after(None), Ok(0));
        assert_eq!(after(Some("x=1")), Ok(0));
        assert_eq!(after(Some("x=1&after=42")), Ok(42));
        assert_eq!(after(Some("after=-1")).unwrap_err().status, 400);
    }

    #[test]
    fn an_event_carries_its_items_index() {
        let item = Item::Paused {
            reason: "forked".into(),
        };
        assert_eq!(
            String::from_utf8(event(7, &item)).unwrap(),
            "id: 7\ndata: {\"type\":\"paused\",\"reason\":\"forked\"}\n\n"
        );
        assert_eq!(
            String::from_utf8(writing(3, "lo")).unwrap(),
            "event: writing\ndata: {\"at\":3,\"text\":\"lo\"}\n\n"
        );
    }
}
