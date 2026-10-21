//! What a `groovy://` link asks for. A link never acts on its own: it fills in the input where
//! the user can see it, and the user presses the button.

/// What a link is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A contact's ticket, to pair with on the Peers screen. Holds the link as it was given.
    Pair(String),
    /// A group's invitation, to join on the Messages screen.
    Join(String),
    /// Just bring the app forward.
    Open,
}

/// The scheme Groovy's links use.
pub const SCHEME: &str = "groovy://";

/// Reads a link. Anything that is not a `groovy://` link gives `None`.
pub fn parse(text: &str) -> Option<Link> {
    let text = text.trim();
    let body = text.strip_prefix(SCHEME)?.trim_end_matches('/');
    if body.is_empty() {
        return Some(Link::Open);
    }
    if body.starts_with("groovygroup1") {
        return Some(Link::Join(body.to_owned()));
    }
    // Device links, and anything else, are not something a link can start from here.
    if body.starts_with("groovylink1") {
        return Some(Link::Open);
    }
    Some(Link::Pair(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_link_keeps_the_text_it_came_with() {
        assert_eq!(
            parse("groovy://abc123"),
            Some(Link::Pair("groovy://abc123".to_owned()))
        );
    }

    #[test]
    fn a_group_invitation_link_joins_the_group() {
        assert_eq!(
            parse("groovy://groovygroup1abc/"),
            Some(Link::Join("groovygroup1abc".to_owned()))
        );
    }

    #[test]
    fn an_empty_link_just_opens_the_app_and_other_text_is_not_a_link() {
        assert_eq!(parse("groovy://"), Some(Link::Open));
        assert_eq!(parse("https://example.com"), None);
        assert_eq!(parse("  groovy://  "), Some(Link::Open));
    }
}
