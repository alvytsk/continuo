pub mod capabilities;
pub mod id;
pub mod metadata;
pub mod source;
pub mod vbr_header;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Episode {
    pub id: id::MediaId,
    pub source: Option<source::SourceLocation>,
}
