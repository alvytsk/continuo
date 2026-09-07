use std::time::Duration;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
}
