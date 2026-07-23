//! The `ents` attribute namespace: presentation policy declared once, on
//! an entity's own fields, and read generically by any surface walking its
//! [`facet::Shape`] — never by matching on the concrete entity type.
//!
//! A separate crate for the same reason `figue`'s own attribute crate is:
//! a macro-expanded `#[macro_export]` macro cannot be referred to by
//! absolute path from the crate that expands it, so the entity crates
//! (`ents-model`, `ents-forge`) could not annotate their own fields if the
//! grammar lived in either of them. Use as `use ents_attrs as ents;`, then
//! `#[facet(ents::head)]` etc.

extern crate self as ents_attrs;

// @relation(model.presentation, scope=file)
facet::define_attr_grammar! {
    ns "ents";
    crate_path ::ents_attrs;

    /// Presentation roles a field declares via `#[facet(ents::...)]`.
    pub enum Attr {
        /// Never rendered generically: identity bound into the refname, or
        /// domain-rendered by a bespoke line.
        Skip,
        /// A porcelain head-line token (values must be single words); also
        /// a leading human list column.
        Head,
        /// A human list column, after the head columns.
        Col,
        /// Omitted from show and porcelain when the value is empty.
        SkipEmpty,
        /// An id value: 20 raw bytes render as hex; abbreviated in human
        /// output, full in porcelain.
        Id,
        /// The message body: the last `field: value` line of show, the
        /// tab-indented block of a porcelain record.
        Body,
        /// A compose field on an action variant: filled by
        /// $GIT_EDITOR/$EDITOR when its flag is omitted.
        Compose,
    }
}
