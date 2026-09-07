use std::path::PathBuf;
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceLocation {
    LocalPath(PathBuf),
    Http(Url),
}
