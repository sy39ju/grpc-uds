// SPDX-License-Identifier: MIT OR Apache-2.0
#![no_std]
#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
// Bindgen copies nghttp2's C doc comments verbatim; they contain bare <URL>s and
// reST markup that rustdoc would otherwise flag. Silence it for generated docs.
#![allow(
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
