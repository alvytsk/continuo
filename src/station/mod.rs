//! Saved radio stations (design doc M8 §4): the durable, user-authored list
//! behind the browser's Radio tab. Deliberately separate from
//! [`crate::subscription`] — a station owns no cache directory, has no
//! episodes and is never refreshed on a schedule — and slugs live in their
//! own namespace, which is safe because no command takes a slug without
//! also naming which list it means.

pub mod model;
pub mod store;
