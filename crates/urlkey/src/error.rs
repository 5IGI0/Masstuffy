//! Error type of [`UrlPattern::new`](crate::UrlPattern::new).

/// Why a [`UrlPattern`](crate::UrlPattern) could not be built from user input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PatternError {
    /// The host is empty, malformed, or contains characters that never appear in a key host.
    #[error("invalid host")]
    InvalidHost,
    /// Subdomain matching was requested on an IPv4 or IPv6 address.
    #[error("subdomain matching is not possible on an IP address")]
    SubdomainsOfIp,
    /// The path does not start with `/`, contains `#`, or a prefix ends with an incomplete
    /// percent-escape.
    #[error(
        "invalid path: must start with '/', contain no '#', and a prefix must not end with an incomplete percent-escape"
    )]
    InvalidPath,
    /// A query prefix in a path pattern ends in an incomplete escape or has unsorted complete pairs.
    #[error("invalid query in path pattern: incomplete escape or pairs not in canonical order")]
    InvalidQuery,
}
