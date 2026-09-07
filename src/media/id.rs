use crate::error::DomainError;
use std::path::{Component, Path, PathBuf};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AbsolutePath(String);

impl AbsolutePath {
    pub fn new(path: PathBuf) -> Result<Self, DomainError> {
        let invalid = |reason| DomainError::InvalidPath {
            path: path.clone(),
            reason,
        };
        let raw = path
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 paths are unsupported"))?;
        if !path.is_absolute() {
            return Err(invalid("path must be absolute"));
        }
        let prefix_len = match path.components().next() {
            Some(Component::Prefix(prefix)) => prefix.as_os_str().len(),
            _ => 0,
        };
        let rooted = &raw[prefix_len..];
        let tail = rooted
            .strip_prefix(std::path::is_separator)
            .ok_or_else(|| invalid("path must include a root separator"))?;
        if !tail.is_empty()
            && tail
                .split(std::path::is_separator)
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(invalid("path contains empty, . or .. components"));
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct NormalizedUrl(String);

impl NormalizedUrl {
    pub fn parse(input: &str) -> Result<Self, DomainError> {
        let mut url = Url::parse(input).map_err(|source| DomainError::InvalidUrl {
            input: input.into(),
            source,
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(DomainError::UnsupportedUrl {
                input: input.into(),
            });
        }
        url.set_fragment(None);
        Ok(Self(url.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct FeedId(String);

impl FeedId {
    pub fn new(value: String) -> Result<Self, DomainError> {
        if value.is_empty() {
            return Err(DomainError::EmptyFeedId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EpisodeKey(EpisodeIdentity);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum EpisodeIdentity {
    Guid(String),
    Url(NormalizedUrl),
}

impl EpisodeKey {
    pub fn resolve(
        guid: Option<&str>,
        enclosure: Option<&Url>,
        link: Option<&Url>,
    ) -> Result<Self, DomainError> {
        if let Some(guid) = guid.filter(|value| !value.is_empty()) {
            return Ok(Self(EpisodeIdentity::Guid(guid.into())));
        }
        let url = enclosure
            .or(link)
            .ok_or(DomainError::MissingEpisodeIdentity)?;
        Ok(Self(EpisodeIdentity::Url(NormalizedUrl::parse(
            url.as_str(),
        )?)))
    }
}
