// Copyright 2013-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Markdown formatting for rustdoc
//!
//! This module implements markdown formatting through the hoedown C-library
//! (bundled into the rust runtime). This module self-contains the C bindings
//! and necessary legwork to render markdown, and exposes all of the
//! functionality through a unit-struct, `Markdown`, which has an implementation
//! of `fmt::Show`. Example usage:
//!
//! ```rust,ignore
//! use rustdoc::html::markdown::Markdown;
//!
//! let s = "My *markdown* _text_";
//! let html = format!("{}", Markdown(s));
//! // ... something using html
//! ```

#![allow(dead_code)]
#![allow(non_camel_case_types)]

use libc;
use std::cell::{RefCell, Cell};
use std::fmt;
use std::slice;
use std::str;
use std::string;
use std::collections::HashMap;

use html::toc::TocBuilder;
use html::highlight;
use html::escape::Escape;
use test;

/// A unit struct which has the `fmt::Show` trait implemented. When
/// formatted, this struct will emit the HTML corresponding to the rendered
/// version of the contained markdown string.
pub struct Markdown<'a>(pub &'a str);
/// A unit struct like `Markdown`, that renders the markdown with a
/// table of contents.
pub struct MarkdownWithToc<'a>(pub &'a str);

static DEF_OUNIT: libc::size_t = 64;
static HOEDOWN_EXT_NO_INTRA_EMPHASIS: libc::c_uint = 1 << 10;
static HOEDOWN_EXT_TABLES: libc::c_uint = 1 << 0;
static HOEDOWN_EXT_FENCED_CODE: libc::c_uint = 1 << 1;
static HOEDOWN_EXT_AUTOLINK: libc::c_uint = 1 << 3;
static HOEDOWN_EXT_STRIKETHROUGH: libc::c_uint = 1 << 4;
static HOEDOWN_EXT_SUPERSCRIPT: libc::c_uint = 1 << 8;
static HOEDOWN_EXT_FOOTNOTES: libc::c_uint = 1 << 2;

static HOEDOWN_EXTENSIONS: libc::c_uint =
    HOEDOWN_EXT_NO_INTRA_EMPHASIS | HOEDOWN_EXT_TABLES |
    HOEDOWN_EXT_FENCED_CODE | HOEDOWN_EXT_AUTOLINK |
    HOEDOWN_EXT_STRIKETHROUGH | HOEDOWN_EXT_SUPERSCRIPT |
    HOEDOWN_EXT_FOOTNOTES;

type hoedown_document = libc::c_void;  // this is opaque to us

struct hoedown_renderer {
    opaque: *mut hoedown_html_renderer_state,
    blockcode: Option<extern "C" fn(*mut hoedown_buffer, *const hoedown_buffer,
                                    *const hoedown_buffer, *mut libc::c_void)>,
    blockquote: Option<extern "C" fn(*mut hoedown_buffer, *const hoedown_buffer,
                                     *mut libc::c_void)>,
    blockhtml: Option<extern "C" fn(*mut hoedown_buffer, *const hoedown_buffer,
                                    *mut libc::c_void)>,
    header: Option<extern "C" fn(*mut hoedown_buffer, *const hoedown_buffer,
                                 libc::c_int, *mut libc::c_void)>,
    other: [libc::size_t, ..28],
}

struct hoedown_html_renderer_state {
    opaque: *mut libc::c_void,
    toc_data: html_toc_data,
    flags: libc::c_uint,
    link_attributes: Option<extern "C" fn(*mut hoedown_buffer,
                                          *const hoedown_buffer,
                                          *mut libc::c_void)>,
}

struct html_toc_data {
    header_count: libc::c_int,
    current_level: libc::c_int,
    level_offset: libc::c_int,
    nesting_level: libc::c_int,
}

struct MyOpaque {
    dfltblk: extern "C" fn(*mut hoedown_buffer, *const hoedown_buffer,
                           *const hoedown_buffer, *mut libc::c_void),
    toc_builder: Option<TocBuilder>,
}

struct hoedown_buffer {
    data: *const u8,
    size: libc::size_t,
    asize: libc::size_t,
    unit: libc::size_t,
}

// hoedown FFI
#[link(name = "hoedown", kind = "static")]
extern {
    fn hoedown_html_renderer_new(render_flags: libc::c_uint,
                                 nesting_level: libc::c_int)
        -> *mut hoedown_renderer;
    fn hoedown_html_renderer_free(renderer: *mut hoedown_renderer);

    fn hoedown_document_new(rndr: *mut hoedown_renderer,
                            extensions: libc::c_uint,
                            max_nesting: libc::size_t) -> *mut hoedown_document;
    fn hoedown_document_render(doc: *mut hoedown_document,
                               ob: *mut hoedown_buffer,
                               document: *const u8,
                               doc_size: libc::size_t);
    fn hoedown_document_free(md: *mut hoedown_document);

    fn hoedown_buffer_new(unit: libc::size_t) -> *mut hoedown_buffer;
    fn hoedown_buffer_puts(b: *mut hoedown_buffer, c: *const libc::c_char);
    fn hoedown_buffer_free(b: *mut hoedown_buffer);

}

/// Returns Some(code) if `s` is a line that should be stripped from
/// documentation but used in example code. `code` is the portion of
/// `s` that should be used in tests. (None for lines that should be
/// left as-is.)
fn stripped_filtered_line<'a>(s: &'a str) -> Option<&'a str> {
    let trimmed = s.trim();
    if trimmed.starts_with("# ") {
        Some(trimmed.slice_from(2))
    } else {
        None
    }
}

local_data_key!(used_header_map: RefCell<HashMap<String, uint>>)
local_data_key!(test_idx: Cell<uint>)
// None == render an example, but there's no crate name
local_data_key!(pub playground_krate: Option<String>)

pub fn render(w: &mut fmt::Formatter, s: &str, print_toc: bool) -> fmt::Result {
    extern fn block(ob: *mut hoedown_buffer, text: *const hoedown_buffer,
                    lang: *const hoedown_buffer, opaque: *mut libc::c_void) {
        unsafe {
            if text.is_null() { return }

            let opaque = opaque as *mut hoedown_html_renderer_state;
            let my_opaque: &MyOpaque = &*((*opaque).opaque as *const MyOpaque);
            slice::raw::buf_as_slice((*text).data, (*text).size as uint, |text| {
                let origtext = str::from_utf8(text).unwrap();
                debug!("docblock: ==============\n{}\n=======", text);
                let mut lines = origtext.lines().filter(|l| {
                    stripped_filtered_line(*l).is_none()
                });
                let text = lines.collect::<Vec<&str>>().connect("\n");

                let buf = hoedown_buffer {
                    data: text.as_bytes().as_ptr(),
                    size: text.len() as libc::size_t,
                    asize: text.len() as libc::size_t,
                    unit: 0,
                };
                let rendered = if lang.is_null() {
                    false
                } else {
                    slice::raw::buf_as_slice((*lang).data,
                                           (*lang).size as uint, |rlang| {
                        let rlang = str::from_utf8(rlang).unwrap();
                        if LangString::parse(rlang).notrust {
                            (my_opaque.dfltblk)(ob, &buf, lang,
                                                opaque as *mut libc::c_void);
                            true
                        } else {
                            false
                        }
                    })
                };

                if !rendered {
                    let mut s = String::new();
                    let id = playground_krate.get().map(|krate| {
                        let idx = test_idx.get().unwrap();
                        let i = idx.get();
                        idx.set(i + 1);

                        let test = origtext.lines().map(|l| {
                            stripped_filtered_line(l).unwrap_or(l)
                        }).collect::<Vec<&str>>().connect("\n");
                        let krate = krate.as_ref().map(|s| s.as_slice());
                        let test = test::maketest(test.as_slice(), krate, false, false);
                        s.push_str(format!("<span id='rust-example-raw-{}' \
                                             class='rusttest'>{}</span>",
                                           i, Escape(test.as_slice())).as_slice());
                        format!("rust-example-rendered-{}", i)
                    });
                    let id = id.as_ref().map(|a| a.as_slice());
                    s.push_str(highlight::highlight(text.as_slice(), None, id)
                                         .as_slice());
                    let output = s.to_c_str();
                    hoedown_buffer_puts(ob, output.as_ptr());
                }
            })
        }
    }

    extern fn header(ob: *mut hoedown_buffer, text: *const hoedown_buffer,
                     level: libc::c_int, opaque: *mut libc::c_void) {
        // hoedown does this, we may as well too
        "\n".with_c_str(|p| unsafe { hoedown_buffer_puts(ob, p) });

        // Extract the text provided
        let s = if text.is_null() {
            "".to_string()
        } else {
            unsafe {
                string::raw::from_buf_len((*text).data, (*text).size as uint)
            }
        };

        // Transform the contents of the header into a hyphenated string
        let id = s.as_slice().words().map(|s| {
            match s.to_ascii_opt() {
                Some(s) => s.to_lower().into_string(),
                None => s.to_string()
            }
        }).collect::<Vec<String>>().connect("-");

        // This is a terrible hack working around how hoedown gives us rendered
        // html for text rather than the raw text.
        let id = id.replace("<code>", "").replace("</code>", "").to_string();

        let opaque = opaque as *mut hoedown_html_renderer_state;
        let opaque = unsafe { &mut *((*opaque).opaque as *mut MyOpaque) };

        // Make sure our hyphenated ID is unique for this page
        let map = used_header_map.get().unwrap();
        let id = match map.borrow_mut().find_mut(&id) {
            None => id,
            Some(a) => { *a += 1; format!("{}-{}", id, *a - 1) }
        };
        map.borrow_mut().insert(id.clone(), 1);

        let sec = match opaque.toc_builder {
            Some(ref mut builder) => {
                builder.push(level as u32, s.clone(), id.clone())
            }
            None => {""}
        };

        // Render the HTML
        let text = format!(r##"<h{lvl} id="{id}" class='section-header'><a
                           href="#{id}">{sec}{}</a></h{lvl}>"##,
                           s, lvl = level, id = id,
                           sec = if sec.len() == 0 {
                               sec.to_string()
                           } else {
                               format!("{} ", sec)
                           });

        text.with_c_str(|p| unsafe { hoedown_buffer_puts(ob, p) });
    }

    unsafe {
        let ob = hoedown_buffer_new(DEF_OUNIT);
        let renderer = hoedown_html_renderer_new(0, 0);
        let mut opaque = MyOpaque {
            dfltblk: (*renderer).blockcode.unwrap(),
            toc_builder: if print_toc {Some(TocBuilder::new())} else {None}
        };
        (*(*renderer).opaque).opaque = &mut opaque as *mut _ as *mut libc::c_void;
        (*renderer).blockcode = Some(block);
        (*renderer).header = Some(header);

        let document = hoedown_document_new(renderer, HOEDOWN_EXTENSIONS, 16);
        hoedown_document_render(document, ob, s.as_ptr(),
                                s.len() as libc::size_t);
        hoedown_document_free(document);

        hoedown_html_renderer_free(renderer);

        let mut ret = match opaque.toc_builder {
            Some(b) => write!(w, "<nav id=\"TOC\">{}</nav>", b.into_toc()),
            None => Ok(())
        };

        if ret.is_ok() {
            ret = slice::raw::buf_as_slice((*ob).data, (*ob).size as uint, |buf| {
                w.write(buf)
            });
        }
        hoedown_buffer_free(ob);
        ret
    }
}

pub fn find_testable_code(doc: &str, tests: &mut ::test::Collector) {
    extern fn block(_ob: *mut hoedown_buffer,
                    text: *const hoedown_buffer,
                    lang: *const hoedown_buffer,
                    opaque: *mut libc::c_void) {
        unsafe {
            if text.is_null() { return }
            let block_info = if lang.is_null() {
                LangString::all_false()
            } else {
                slice::raw::buf_as_slice((*lang).data,
                                       (*lang).size as uint, |lang| {
                    let s = str::from_utf8(lang).unwrap();
                    LangString::parse(s)
                })
            };
            if block_info.notrust { return }
            slice::raw::buf_as_slice((*text).data, (*text).size as uint, |text| {
                let opaque = opaque as *mut hoedown_html_renderer_state;
                let tests = &mut *((*opaque).opaque as *mut ::test::Collector);
                let text = str::from_utf8(text).unwrap();
                let mut lines = text.lines().map(|l| {
                    stripped_filtered_line(l).unwrap_or(l)
                });
                let text = lines.collect::<Vec<&str>>().connect("\n");
                tests.add_test(text.to_string(),
                               block_info.should_fail, block_info.no_run,
                               block_info.ignore, block_info.test_harness);
            })
        }
    }

    extern fn header(_ob: *mut hoedown_buffer,
                     text: *const hoedown_buffer,
                     level: libc::c_int, opaque: *mut libc::c_void) {
        unsafe {
            let opaque = opaque as *mut hoedown_html_renderer_state;
            let tests = &mut *((*opaque).opaque as *mut ::test::Collector);
            if text.is_null() {
                tests.register_header("", level as u32);
            } else {
                slice::raw::buf_as_slice((*text).data, (*text).size as uint, |text| {
                    let text = str::from_utf8(text).unwrap();
                    tests.register_header(text, level as u32);
                })
            }
        }
    }

    unsafe {
        let ob = hoedown_buffer_new(DEF_OUNIT);
        let renderer = hoedown_html_renderer_new(0, 0);
        (*renderer).blockcode = Some(block);
        (*renderer).header = Some(header);
        (*(*renderer).opaque).opaque = tests as *mut _ as *mut libc::c_void;

        let document = hoedown_document_new(renderer, HOEDOWN_EXTENSIONS, 16);
        hoedown_document_render(document, ob, doc.as_ptr(),
                                doc.len() as libc::size_t);
        hoedown_document_free(document);

        hoedown_html_renderer_free(renderer);
        hoedown_buffer_free(ob);
    }
}

#[deriving(Eq, PartialEq, Clone, Show)]
struct LangString {
    should_fail: bool,
    no_run: bool,
    ignore: bool,
    notrust: bool,
    test_harness: bool,
}

impl LangString {
    fn all_false() -> LangString {
        LangString {
            should_fail: false,
            no_run: false,
            ignore: false,
            notrust: false,
            test_harness: false,
        }
    }

    fn parse(string: &str) -> LangString {
        let mut seen_rust_tags = false;
        let mut seen_other_tags = false;
        let mut data = LangString::all_false();

        let mut tokens = string.as_slice().split(|c: char|
            !(c == '_' || c == '-' || c.is_alphanumeric())
        );

        for token in tokens {
            match token {
                "" => {},
                "should_fail" => { data.should_fail = true; seen_rust_tags = true; },
                "no_run" => { data.no_run = true; seen_rust_tags = true; },
                "ignore" => { data.ignore = true; seen_rust_tags = true; },
                "notrust" => { data.notrust = true; seen_rust_tags = true; },
                "rust" => { data.notrust = false; seen_rust_tags = true; },
                "test_harness" => { data.test_harness = true; seen_rust_tags = true; }
                _ => { seen_other_tags = true }
            }
        }

        data.notrust |= seen_other_tags && !seen_rust_tags;

        data
    }
}

/// By default this markdown renderer generates anchors for each header in the
/// rendered document. The anchor name is the contents of the header separated
/// by hyphens, and a task-local map is used to disambiguate among duplicate
/// headers (numbers are appended).
///
/// This method will reset the local table for these headers. This is typically
/// used at the beginning of rendering an entire HTML page to reset from the
/// previous state (if any).
pub fn reset_headers() {
    used_header_map.replace(Some(RefCell::new(HashMap::new())));
    test_idx.replace(Some(Cell::new(0)));
}

impl<'a> fmt::Show for Markdown<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let Markdown(md) = *self;
        // This is actually common enough to special-case
        if md.len() == 0 { return Ok(()) }
        render(fmt, md.as_slice(), false)
    }
}

impl<'a> fmt::Show for MarkdownWithToc<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let MarkdownWithToc(md) = *self;
        render(fmt, md.as_slice(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::LangString;

    #[test]
    fn test_lang_string_parse() {
        fn t(s: &str,
             should_fail: bool, no_run: bool, ignore: bool, notrust: bool, test_harness: bool) {
            assert_eq!(LangString::parse(s), LangString {
                should_fail: should_fail,
                no_run: no_run,
                ignore: ignore,
                notrust: notrust,
                test_harness: test_harness,
            })
        }

        t("", false,false,false,false,false);
        t("rust", false,false,false,false,false);
        t("sh", false,false,false,true,false);
        t("notrust", false,false,false,true,false);
        t("ignore", false,false,true,false,false);
        t("should_fail", true,false,false,false,false);
        t("no_run", false,true,false,false,false);
        t("test_harness", false,false,false,false,true);
        t("{.no_run .example}", false,true,false,false,false);
        t("{.sh .should_fail}", true,false,false,false,false);
        t("{.example .rust}", false,false,false,false,false);
        t("{.test_harness .rust}", false,false,false,false,true);
    }
}
