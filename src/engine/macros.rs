//! Annotation-macro neutralization for the syntactic backend.
//!
//! tree-sitter-cpp has no preprocessor. An annotation macro sitting between a
//! return type and a function name — `static CWideStr LIB_API Foo(...)`,
//! the classic dllimport/dllexport pattern — therefore derails its parse: the
//! macro is mistaken for a member name and error recovery splits or mangles the
//! declaration. In the pure-virtual interface case (`... LIB_API Foo(...) = 0;`)
//! one overload even reparses as a `function_definition`, which `find-decl`
//! deliberately skips — so an overload silently vanishes.
//!
//! The fix is to blank those macros (replace their bytes with ASCII spaces)
//! *before* parsing. Blanking is **length-preserving**, so every byte offset in
//! the resulting tree still indexes the original buffer unchanged — callers keep
//! slicing the original source, and the macro text remains visible in output.
//!
//! ## Strictness: only blank *confirmed* macros
//!
//! To avoid blanking a genuine type or constant that happens to look like an
//! annotation, [`neutralize`] blanks only names it can prove are macros:
//!  * `global` — the user's `--empty-macro` list, blanked at every occurrence
//!    (full `-DNAME=` semantics; trusted);
//!  * `annotation` — names harvested from `#define` directives (project-wide and
//!    the file's own), blanked only in the annotation slot `TYPE MACRO NAME (`.
//!    A value macro such as `#define PI 3.14` is in this set but never appears in
//!    that slot, so it is never touched.
//!
//! An UPPER_CASE token in the annotation slot that is *not* a confirmed macro
//! (e.g. a macro from an unscanned system header) is left alone; callers can
//! surface it via [`unconfirmed_annotations`] to suggest `--empty-macro NAME`.

use std::collections::HashSet;

/// A lexical token of interest. Whitespace, comments, string/char literals, and
/// preprocessor lines are consumed by the scanner and never emitted.
#[derive(Clone, Copy)]
enum Tok {
    Ident { start: usize, end: usize, all_caps: bool },
    /// `(` — opens a parameter list.
    Open,
    /// `*`, `&`, `>`, or `::` — a token that can terminate a return type.
    TypeIsh,
    /// Any other punctuation/number; tracked only as a boundary.
    Other,
}

/// Produce a length-preserving copy of `src` with confirmed macros blanked, or
/// `None` if nothing was blanked. See the module docs for `global`/`annotation`.
pub fn neutralize(
    src: &[u8],
    global: &HashSet<String>,
    annotation: &HashSet<String>,
) -> Option<Vec<u8>> {
    if global.is_empty() && annotation.is_empty() {
        return None;
    }
    let toks = scan(src);
    let mut blanks: Vec<(usize, usize)> = Vec::new();

    for (i, t) in toks.iter().enumerate() {
        if let Tok::Ident { start, end, .. } = *t {
            let Ok(name) = std::str::from_utf8(&src[start..end]) else {
                continue;
            };
            if global.contains(name) {
                // Trusted user macro: blank every occurrence.
                blanks.push((start, end));
            } else if annotation.contains(name)
                && prev_is_typeish(&toks, i)
                && next_is_ident_open(&toks, i)
            {
                // Confirmed macro in annotation position only.
                blanks.push((start, end));
            }
        }
    }

    if blanks.is_empty() {
        return None;
    }
    let mut out = src.to_vec();
    for (s, e) in blanks {
        out[s..e].fill(b' ');
    }
    Some(out)
}

/// UPPER_CASE tokens sitting in the annotation slot that are *not* confirmed
/// macros (neither in `confirmed` nor `#define`d in this file). These are the
/// likely culprits when an overload is hidden but no macro was blanked — the
/// caller suggests `--empty-macro NAME` for each. Deduplicated, in first-seen
/// order.
pub fn unconfirmed_annotations(src: &[u8], confirmed: &HashSet<String>) -> Vec<String> {
    let mut known = confirmed.clone();
    collect_defines(src, &mut known);

    let toks = scan(src);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Ident { start, end, all_caps: true } = *t
            && prev_is_typeish(&toks, i)
            && next_is_ident_open(&toks, i)
            && let Ok(name) = std::str::from_utf8(&src[start..end])
            && !known.contains(name)
            && seen.insert(name.to_string())
        {
            out.push(name.to_string());
        }
    }
    out
}

/// Harvest macro names from `#define` directives in `src` into `out`.
/// Handles leading whitespace, `#  define`, and function-like `#define F(x)`.
pub fn collect_defines(src: &[u8], out: &mut HashSet<String>) {
    for line in src.split(|&b| b == b'\n') {
        let line = line.trim_ascii_start();
        let Some(rest) = line.strip_prefix(b"#") else {
            continue;
        };
        let rest = rest.trim_ascii_start();
        let Some(rest) = rest.strip_prefix(b"define") else {
            continue;
        };
        // `define` must be followed by whitespace (not `defined`/`definer`).
        let after = match rest.first() {
            Some(b) if b.is_ascii_whitespace() => rest.trim_ascii_start(),
            _ => continue,
        };
        let name_len = after
            .iter()
            .take_while(|&&b| is_ident_continue(b))
            .count();
        if name_len > 0
            && is_ident_start(after[0])
            && let Ok(name) = std::str::from_utf8(&after[..name_len])
        {
            out.insert(name.to_string());
        }
    }
}

/// The previous emitted token can terminate a return type (so the current token
/// is *not* the return type itself but an annotation following it).
fn prev_is_typeish(toks: &[Tok], i: usize) -> bool {
    i > 0 && matches!(toks[i - 1], Tok::Ident { .. } | Tok::TypeIsh)
}

/// The next two tokens are `IDENT (` — i.e. the current token sits immediately
/// before a function name and its parameter list.
fn next_is_ident_open(toks: &[Tok], i: usize) -> bool {
    matches!(toks.get(i + 1), Some(Tok::Ident { .. }))
        && matches!(toks.get(i + 2), Some(Tok::Open))
}

/// Lexically scan `src` into [`Tok`]s, skipping whitespace, comments,
/// string/char literals, and preprocessor (`#…`) lines.
fn scan(src: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let n = src.len();
    let mut i = 0;
    // Whether the only thing seen since the last newline is whitespace — used to
    // recognize a preprocessor directive at line start.
    let mut at_line_start = true;

    while i < n {
        let b = src[i];
        match b {
            b'\n' => {
                at_line_start = true;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            b'#' if at_line_start => {
                // Skip the whole directive, honoring `\`-newline continuations.
                i = skip_pp_line(src, i);
            }
            b'/' if i + 1 < n && src[i + 1] == b'/' => {
                while i < n && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && src[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(src[i] == b'*' && src[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                at_line_start = false;
            }
            b'"' => {
                i = skip_quoted(src, i, b'"');
                at_line_start = false;
            }
            b'\'' => {
                i = skip_quoted(src, i, b'\'');
                at_line_start = false;
            }
            b'(' => {
                toks.push(Tok::Open);
                i += 1;
                at_line_start = false;
            }
            b'*' | b'&' | b'>' => {
                toks.push(Tok::TypeIsh);
                i += 1;
                at_line_start = false;
            }
            b':' if i + 1 < n && src[i + 1] == b':' => {
                toks.push(Tok::TypeIsh);
                i += 2;
                at_line_start = false;
            }
            _ if is_ident_start(b) => {
                let start = i;
                while i < n && is_ident_continue(src[i]) {
                    i += 1;
                }
                toks.push(Tok::Ident {
                    start,
                    end: i,
                    all_caps: is_all_caps(&src[start..i]),
                });
                at_line_start = false;
            }
            _ => {
                toks.push(Tok::Other);
                i += 1;
                at_line_start = false;
            }
        }
    }
    toks
}

/// Skip a preprocessor line starting at `i` (the `#`), following `\`-newline
/// continuations. Returns the index just past the directive.
fn skip_pp_line(src: &[u8], mut i: usize) -> usize {
    let n = src.len();
    while i < n {
        if src[i] == b'\n' {
            return i; // leave the newline for the main loop to reset state
        }
        if src[i] == b'\\' && i + 1 < n && src[i + 1] == b'\n' {
            i += 2; // line continuation
            continue;
        }
        i += 1;
    }
    i
}

/// Skip a `"`- or `'`-delimited literal starting at the opening quote `i`,
/// honoring backslash escapes. Returns the index just past the closing quote.
fn skip_quoted(src: &[u8], mut i: usize, quote: u8) -> usize {
    let n = src.len();
    i += 1; // opening quote
    while i < n {
        match src[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            b'\n' => return i, // unterminated — stop at the newline
            _ => i += 1,
        }
    }
    i
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// An "annotation-like" identifier: length ≥ 2, only `[A-Z0-9_]`, with at least
/// one letter. Matches `LIB_API`, `WINAPI`, `DLLEXPORT`, `HRESULT`; rejects
/// lower/mixed-case names and bare numbers. Used only to flag *unconfirmed*
/// annotation tokens for the `--empty-macro` hint (never to blank).
fn is_all_caps(name: &[u8]) -> bool {
    name.len() >= 2
        && name.iter().any(|b| b.is_ascii_uppercase())
        && name
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Blank using only an annotation (confirmed-macro) set.
    fn neu_annot(src: &str, names: &[&str]) -> Option<String> {
        neutralize(src.as_bytes(), &HashSet::new(), &set(names))
            .map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn blanks_confirmed_macro_in_annotation_position() {
        let src = "static CWideStr LIB_API Foo(int a);";
        let out = neu_annot(src, &["LIB_API"]).unwrap();
        assert_eq!(out.len(), src.len());
        assert!(!out.contains("LIB_API"));
        assert!(out.starts_with("static CWideStr "));
        assert!(out.ends_with(" Foo(int a);"));
        assert_eq!(out.replace(' ', ""), "staticCWideStrFoo(inta);");
    }

    #[test]
    fn does_not_blank_unconfirmed_uppercase_token() {
        // UPPER_CASE in annotation position, but not a confirmed macro → strict.
        assert!(neu_annot("static CStr LIB_API Foo(int a);", &[]).is_none());
    }

    #[test]
    fn does_not_blank_confirmed_macro_outside_annotation_position() {
        // A confirmed (annotation-set) macro used as a value is never touched —
        // only the annotation slot is blanked.
        assert!(neu_annot("int x = PI * r;", &["PI"]).is_none());
    }

    #[test]
    fn global_macro_is_blanked_everywhere() {
        let src = "class LibExport Widget;";
        let out = String::from_utf8(
            neutralize(src.as_bytes(), &set(&["LibExport"]), &HashSet::new()).unwrap(),
        )
        .unwrap();
        assert_eq!(out, "class           Widget;");
    }

    #[test]
    fn leaves_lone_uppercase_return_type_alone() {
        // `BOOL` is the return type (not preceded by another type); even if it
        // were confirmed, it is not in annotation position.
        assert!(neu_annot("BOOL Foo(int a);", &["BOOL"]).is_none());
    }

    #[test]
    fn collect_defines_harvests_names() {
        let mut out = HashSet::new();
        collect_defines(
            b"#define LIB_API __declspec(dllimport)\n#  define FOO(x) ((x)+1)\nint defined_elsewhere;\n",
            &mut out,
        );
        assert!(out.contains("LIB_API"));
        assert!(out.contains("FOO"));
        // `defined_elsewhere` is not a #define.
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn unconfirmed_annotations_reports_uppercase_not_defined() {
        let src = "static CStr LIB_API Foo(int a);\nstatic CStr KNOWN Bar();";
        let confirmed = set(&["KNOWN"]);
        let got = unconfirmed_annotations(src.as_bytes(), &confirmed);
        assert_eq!(got, vec!["LIB_API".to_string()]);
    }

    #[test]
    fn unconfirmed_annotations_respects_file_defines() {
        // The macro is #define'd in the same file → confirmed, not reported.
        let src = "#define LIB_API\nstatic CStr LIB_API Foo(int a);";
        assert!(unconfirmed_annotations(src.as_bytes(), &HashSet::new()).is_empty());
    }

    #[test]
    fn does_not_blank_inside_strings_or_comments() {
        assert!(neu_annot("const char* s = \"int M Foo(\"; // int M Bar(", &["M"]).is_none());
    }
}
