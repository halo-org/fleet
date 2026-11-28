use std::collections::BTreeSet;

/// What a Bearer challenge asks for.
///
/// RFC 6750 defines `error` and `scope`, and RFC 9728 defines `resource_metadata`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    /// The error code.
    pub error: Option<String>,
    /// The scopes the request needs.
    pub scopes: Vec<String>,
    /// The URL of the protected resource's metadata.
    pub resource_metadata: Option<String>,
}

impl Challenge {
    /// Reads the first Bearer challenge of a `WWW-Authenticate` value, defined by
    /// RFC 9110.
    ///
    /// Returns nothing for a value that cannot be parsed or has no Bearer challenge.
    pub fn parse(value: &str) -> Option<Self> {
        let mut bearer = None;
        let mut rest = value;
        loop {
            rest = rest.trim_start_matches([' ', '\t', ',']);
            if rest.is_empty() {
                break;
            }
            let (scheme, after) = token(rest)?;
            let (parameters, after) = credentials(after)?;
            rest = after;
            if bearer.is_none() && scheme.eq_ignore_ascii_case("bearer") {
                bearer = Some(parameters);
            }
        }
        let mut challenge = Self::default();
        for (name, value) in bearer? {
            match name.to_ascii_lowercase().as_str() {
                "error" => challenge.error = Some(value),
                "scope" => {
                    challenge.scopes = value.split_ascii_whitespace().map(Into::into).collect();
                }
                "resource_metadata" => challenge.resource_metadata = Some(value),
                _ => {}
            }
        }
        Some(challenge)
    }
}

/// Reads what follows a challenge's scheme, a token68 or parameters with distinct
/// names, up to the next challenge.
fn credentials(input: &str) -> Option<(Vec<(&str, String)>, &str)> {
    let mut parameters = Vec::new();
    let mut rest = input;
    let spaced = input.trim_start_matches(' ');
    if spaced.len() < input.len() {
        rest = spaced;
        let mut next = parameter(spaced.trim_start_matches([' ', '\t', ',']));
        if next.is_none() && !ows(spaced).is_empty() && !ows(spaced).starts_with(',') {
            rest = token68(spaced)?;
        }
        while let Some((found, after)) = next {
            parameters.push(found);
            rest = after;
            next = ows(rest)
                .strip_prefix(',')
                .and_then(|after| parameter(after.trim_start_matches([' ', '\t', ','])));
        }
    }
    let mut names = BTreeSet::new();
    if !parameters
        .iter()
        .all(|(name, _)| names.insert(name.to_ascii_lowercase()))
    {
        return None;
    }
    let rest = ows(rest);
    (rest.is_empty() || rest.starts_with(',')).then_some((parameters, rest))
}

/// Reads a parameter whose value is a token or a quoted string.
fn parameter(input: &str) -> Option<((&str, String), &str)> {
    let (name, rest) = token(input)?;
    let rest = ows(ows(rest).strip_prefix('=')?);
    let (value, rest) = match rest.strip_prefix('"') {
        Some(quoted) => quoted_string(quoted)?,
        None => token(rest).map(|(value, rest)| (value.to_owned(), rest))?,
    };
    Some(((name, value), rest))
}

/// Reads the rest of a quoted string after its opening quote, removing escapes.
fn quoted_string(input: &str) -> Option<(String, &str)> {
    let text = |character: char| {
        matches!(character, '\t' | ' ') || character.is_ascii_graphic() || !character.is_ascii()
    };
    let mut value = String::new();
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some((value, characters.as_str())),
            '\\' => value.push(characters.next().filter(|escaped| text(*escaped))?),
            character if text(character) => value.push(character),
            _ => return None,
        }
    }
    None
}

fn token(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start_matches(|character: char| {
        character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character)
    });
    let token = input.strip_suffix(rest).filter(|token| !token.is_empty())?;
    Some((token, rest))
}

fn token68(input: &str) -> Option<&str> {
    let rest = input.trim_start_matches(|character: char| {
        character.is_ascii_alphanumeric() || "-._~+/".contains(character)
    });
    (rest.len() < input.len()).then(|| rest.trim_start_matches('='))
}

fn ows(input: &str) -> &str {
    input.trim_start_matches([' ', '\t'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(
        error: Option<&str>,
        scopes: &[&str],
        resource_metadata: Option<&str>,
    ) -> Challenge {
        Challenge {
            error: error.map(Into::into),
            scopes: scopes.iter().copied().map(Into::into).collect(),
            resource_metadata: resource_metadata.map(Into::into),
        }
    }

    #[test]
    fn the_first_bearer_challenge_is_read() {
        let metadata = "https://api.example.com/.well-known/oauth-protected-resource";
        for (value, expected) in [
            (
                format!(
                    "Bearer error=\"insufficient_scope\", scope=\"read write\", resource_metadata=\"{metadata}\""
                ),
                challenge(
                    Some("insufficient_scope"),
                    &["read", "write"],
                    Some(metadata),
                ),
            ),
            (
                "Basic realm=\"a, b\", Bearer scope=\"read\"".into(),
                challenge(None, &["read"], None),
            ),
            (
                "Bearer realm=\"say \\\"hi\\\", \\ok\", error=invalid_token".into(),
                challenge(Some("invalid_token"), &[], None),
            ),
            (
                "Negotiate abc+/def==, Bearer error = \"invalid_token\"".into(),
                challenge(Some("invalid_token"), &[], None),
            ),
            ("Bearer abc==".into(), challenge(None, &[], None)),
            (
                "bEaReR ERROR=\"invalid_token\", Scope=\"Read\"".into(),
                challenge(Some("invalid_token"), &["Read"], None),
            ),
            (
                "Bearer scope=\"first\", Bearer scope=\"second\"".into(),
                challenge(None, &["first"], None),
            ),
            (
                ", Basic realm=\"x\" ,, Bearer,  DPoP algs=\"ES256\" ,".into(),
                challenge(None, &[], None),
            ),
            (
                "Bearer realm=example,,error=invalid_token,\tscope=\" a  b \"".into(),
                challenge(Some("invalid_token"), &["a", "b"], None),
            ),
            (
                "Bearer , scope=read, Basic , Negotiate".into(),
                challenge(None, &["read"], None),
            ),
        ] {
            assert_eq!(Challenge::parse(&value), Some(expected), "{value}");
        }
    }

    #[test]
    fn malformed_values_and_other_schemes_have_no_challenge() {
        for value in [
            "",
            " , ",
            "Basic realm=\"x\"",
            "Bearer error=\"unterminated",
            "Bearer error==x",
            "Bearer scope=\"a\" error=\"b\"",
            "Bearer error=\"a\", ERROR=\"b\"",
            "Basic realm=a, realm=b, Bearer scope=write",
            "Bearer error=\"bad\u{1}\"",
            "Bearer error=\"bad\\\u{1}\"",
            "\"Bearer\" error=\"x\"",
            "Bearer error=\"x\", Basic realm=\"unterminated",
            "Bearer abc==def",
            "=x",
        ] {
            assert_eq!(Challenge::parse(value), None, "{value}");
        }
    }
}
