//! vtmux: `libghostty-vt-sys/src/bindings.rs` matches the C headers it was
//! built against, checked by the C compiler. Every `extern "C"` function is
//! declared again in C from its Rust signature (a different signature is
//! `conflicting types`). Every callback type, struct and union field is
//! compared with `__builtin_types_compatible_p`, and every size, alignment
//! and offset `bindings.rs` asserts is a `_Static_assert`. `bindings.rs`
//! is edited by hand when libghostty-vt moves to a new release (VENDOR.md,
//! *Bumping libghostty-vt*), and a Rust declaration that disagrees with C
//! still compiles and links: this is what catches it. Needs `zig` on PATH
//! (`zig cc`), as the build does.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// The bindings with doc attributes dropped (they hold arbitrary text) and
/// all whitespace runs made one space.
fn bindings_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../libghostty-vt-sys/src/bindings.rs");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("#[") && t.ends_with(']')) && !t.starts_with("//")
        })
        .collect();
    kept.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text between the bracket at `open` and its match, and the index
/// just past the match.
fn balanced(s: &str, open: usize) -> (&str, usize) {
    let bytes = s.as_bytes();
    let (o, c) = match bytes[open] {
        b'(' => (b'(', b')'),
        b'{' => (b'{', b'}'),
        b'<' => (b'<', b'>'),
        b'[' => (b'[', b']'),
        other => panic!("not a bracket: {}", other as char),
    };
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        // `->` is not a closing angle bracket.
        if b == b'>' && o == b'<' && i > 0 && bytes[i - 1] == b'-' {
            continue;
        }
        if b == o {
            depth += 1;
        } else if b == c {
            depth -= 1;
            if depth == 0 {
                return (&s[open + 1..i], i + 1);
            }
        }
    }
    panic!("unbalanced at {open}");
}

/// Split `s` at commas outside brackets.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

struct Names {
    enums: HashSet<String>,
    /// `pub const NAME: usize = N;`, for array lengths.
    lengths: std::collections::HashMap<String, String>,
}

/// The C type, as a C type name, of the Rust type `t`. Compound types are
/// spelled with `__typeof__` so that no declarator syntax is needed.
fn c_type(t: &str, names: &Names) -> String {
    // rustfmt leaves a trailing comma in a multi-line `Option<...>`.
    let t = t.trim().trim_end_matches(',').trim();
    if let Some(rest) = t.strip_prefix("*const ") {
        return format!("__typeof__({}) const *", c_type(rest, names));
    }
    if let Some(rest) = t.strip_prefix("*mut ") {
        return format!("__typeof__({}) *", c_type(rest, names));
    }
    if t.starts_with('[') {
        let (inner, _) = balanced(t, 0);
        let (elem, n) = inner.rsplit_once(';').expect("array length");
        let n = n.trim().trim_end_matches("usize");
        let n = names.lengths.get(n).map_or(n, String::as_str);
        return format!("__typeof__({})[{n}]", c_type(elem, names));
    }
    if let Some(rest) = t
        .strip_prefix("::std::option::Option<")
        .or_else(|| t.strip_prefix("Option<"))
    {
        let inner = rest
            .strip_suffix('>')
            .expect("Option<...>")
            .trim()
            .trim_end_matches(',');
        return fn_pointer(inner, names);
    }
    if let Some(module) = t.strip_suffix("::Type") {
        assert!(
            names.enums.contains(module),
            "{module}::Type is no enum module"
        );
        return format!("Ghostty{module}");
    }
    let t = t.strip_prefix("::std::os::raw::").unwrap_or(t);
    let t = t.strip_prefix("::std::ffi::").unwrap_or(t);
    match t {
        "c_char" => "char",
        "c_schar" => "signed char",
        "c_uchar" => "unsigned char",
        "c_short" => "short",
        "c_ushort" => "unsigned short",
        "c_int" => "int",
        "c_uint" => "unsigned int",
        "c_long" => "long",
        "c_ulong" => "unsigned long",
        "c_longlong" => "long long",
        "c_ulonglong" => "unsigned long long",
        "c_void" => "void",
        "u8" => "uint8_t",
        "u16" => "uint16_t",
        "u32" => "uint32_t",
        "u64" => "uint64_t",
        "i8" => "int8_t",
        "i16" => "int16_t",
        "i32" => "int32_t",
        "i64" => "int64_t",
        "usize" => "size_t",
        "isize" => "ptrdiff_t",
        "bool" => "bool",
        "f32" => "float",
        "f64" => "double",
        // Opaque handles point to a C struct with no typedef of its own.
        name if name.ends_with("Impl") => return format!("struct Ghostty{name}"),
        name => return format!("Ghostty{name}"),
    }
    .to_owned()
}

/// `unsafe extern "C" fn(args) -> ret` as a C function pointer type.
fn fn_pointer(t: &str, names: &Names) -> String {
    let t = t.trim();
    let rest = t
        .strip_prefix("unsafe extern \"C\" fn")
        .unwrap_or_else(|| panic!("not a C function pointer: {t}"));
    let open = rest.find('(').expect("parameters");
    let (params, after) = balanced(rest, open);
    let ret = rest[after..].trim().strip_prefix("->").map(str::trim);
    format!(
        "__typeof__({} (*)({}))",
        ret.map_or("void".to_owned(), |r| c_type(r, names)),
        c_params(params, names)
    )
}

fn c_params(params: &str, names: &Names) -> String {
    let list: Vec<String> = split_top(params)
        .into_iter()
        .map(|p| {
            let ty = p.split_once(':').map_or(p, |(_, ty)| ty);
            c_type(ty, names)
        })
        .collect();
    if list.is_empty() {
        "void".to_owned()
    } else {
        list.join(", ")
    }
}

/// A Rust field name as C spells it: bindgen appends `_` to keywords.
fn c_field(name: &str) -> &str {
    const KEYWORDS: &[&str] = &[
        "ref", "type", "fn", "match", "loop", "move", "mod", "impl", "in", "box", "self", "where",
        "use",
    ];
    match name.strip_suffix('_') {
        Some(base) if KEYWORDS.contains(&base) => base,
        _ => name,
    }
}

/// The C file that restates `src` against the headers.
fn check_file(src: &str) -> (String, usize) {
    let mut names = Names {
        enums: HashSet::new(),
        lengths: std::collections::HashMap::new(),
    };
    for part in src.split("pub const ").skip(1) {
        if let Some((name, rest)) = part.split_once(": usize = ") {
            let value = rest.split(';').next().unwrap().trim();
            names.lengths.insert(name.to_owned(), value.to_owned());
        }
    }
    let mut at = 0;
    while let Some(i) = src[at..].find("pub mod ") {
        let start = at + i + "pub mod ".len();
        let end = start + src[start..].find(' ').unwrap();
        names.enums.insert(src[start..end].to_owned());
        at = end;
    }

    let mut c = String::from(
        "#include <stdbool.h>\n#include <stddef.h>\n#include <stdint.h>\n#include <ghostty/vt.h>\n\
         #define SAME(a, b, what) _Static_assert(__builtin_types_compatible_p(a, b), what)\n",
    );
    let mut checks = 0usize;

    // Functions.
    at = 0;
    while let Some(i) = src[at..].find("pub fn ") {
        let start = at + i + "pub fn ".len();
        let open = start + src[start..].find('(').unwrap();
        let name = &src[start..open];
        let (params, after) = balanced(src, open);
        let tail = &src[after..after + src[after..].find(';').unwrap()];
        let ret = tail.trim().strip_prefix("->").map(str::trim);
        writeln!(
            c,
            "{} {name}({});",
            ret.map_or("void".to_owned(), |r| c_type(r, &names)),
            c_params(params, &names)
        )
        .unwrap();
        checks += 1;
        at = after;
    }

    // Callback types.
    at = 0;
    while let Some(i) = src[at..].find("pub type ") {
        let start = at + i + "pub type ".len();
        let eq = start + src[start..].find(" = ").unwrap();
        let name = &src[start..eq];
        let end = eq + src[eq..].find(';').unwrap();
        let value = src[eq + 3..end].trim();
        at = end;
        if name == "Type" {
            continue; // an enum module's own type
        }
        let ty = c_type(value, &names);
        writeln!(c, "SAME(Ghostty{name}, {ty}, \"type {name}\");").unwrap();
        checks += 1;
    }

    // Struct and union fields.
    for kind in ["pub struct ", "pub union "] {
        at = 0;
        while let Some(i) = src[at..].find(kind) {
            let start = at + i + kind.len();
            let open = start + src[start..].find('{').unwrap();
            let name = src[start..open].trim();
            let (body, after) = balanced(src, open);
            at = after;
            for field in split_top(body) {
                let field = field.strip_prefix("pub ").unwrap_or(field);
                let (fname, ty) = field.split_once(':').expect("field");
                let fname = c_field(fname.trim());
                if fname.starts_with('_') {
                    continue; // opaque handles' `_unused`
                }
                let ty = c_type(ty, &names);
                writeln!(
                    c,
                    "SAME(__typeof__(((Ghostty{name} *)0)->{fname}), {ty}, \"{name}.{fname}\");"
                )
                .unwrap();
                checks += 1;
            }
        }
    }

    // The layouts bindings.rs asserts, now against C.
    for (label, what) in [
        ("[\"Size of ", "sizeof"),
        ("[\"Alignment of ", "_Alignof"),
        ("[\"Offset of field: ", "offsetof"),
        ("[\"Offset of ", "offsetof"),
    ] {
        at = 0;
        while let Some(i) = src[at..].find(label) {
            let start = at + i + label.len();
            if label == "[\"Offset of " && src[start..].starts_with("field: ") {
                at = start;
                continue;
            }
            let end = start + src[start..].find('"').unwrap();
            let subject = &src[start..end];
            let close = end + src[end..].find("usize]").unwrap();
            let n: usize = src[..close]
                .rsplit(|ch: char| !ch.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .unwrap();
            at = close;
            let expr = if what == "offsetof" {
                let (st, f) = subject.split_once("::").expect("Type::field");
                format!("offsetof(Ghostty{st}, {})", c_field(f))
            } else {
                format!("{what}(Ghostty{subject})")
            };
            writeln!(c, "_Static_assert({expr} == {n}, \"{what} {subject}\");").unwrap();
            checks += 1;
        }
    }
    (c, checks)
}

#[test]
fn bindings_match_the_c_headers() {
    let include = libghostty_vt::ffi::INCLUDE_DIR
        .expect("the sys crate built Ghostty itself, so it knows its headers");
    let (c, checks) = check_file(&bindings_source());
    assert!(checks > 1000, "only {checks} checks: the parser lost track");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ghostty-abi");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("abi.c");
    std::fs::write(&file, &c).unwrap();
    let out = Command::new("zig")
        .args([
            "cc",
            "-std=gnu11",
            "-c",
            "-Werror=incompatible-pointer-types",
            "-ferror-limit=0",
        ])
        .arg("-o")
        .arg(dir.join("abi.o"))
        .arg("-I")
        .arg(include)
        .arg(&file)
        .env("ZIG_GLOBAL_CACHE_DIR", dir.join("zig-cache"))
        .output()
        .expect("`zig` on PATH (the build needs it too)");
    let errors = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "bindings.rs disagrees with {include} in {} of {checks} checks:\n{errors}",
        errors.matches("error:").count()
    );
}
