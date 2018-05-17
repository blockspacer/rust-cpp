//! This crate is the `cpp` cargo build script implementation. It is useless
//! without the companion crates `cpp`, and `cpp_macro`.
//!
//! For more information, see the [`cpp` crate module level
//! documentation](https://docs.rs/cpp).

extern crate cpp_common;

extern crate cpp_synom as synom;

extern crate cpp_syn as syn;

extern crate cpp_synmap;

extern crate cc;

#[macro_use]
extern crate lazy_static;

use std::env;
use std::path::{Path, PathBuf};
use std::fs::{create_dir, remove_dir_all, File};
use std::io::prelude::*;
use syn::visit::Visitor;
use syn::{Mac, Span, Spanned, DUMMY_SPAN};
use cpp_common::{parsing, Capture, Closure, ClosureSig, Macro, Class, LIB_NAME, STRUCT_METADATA_MAGIC,
                 VERSION, flags};
use cpp_synmap::SourceMap;

fn warnln_impl(a: String) {
    for s in a.lines() {
        println!("cargo:warning={}", s);
    }
}

macro_rules! warnln {
    ($($all:tt)*) => {
        $crate::warnln_impl(format!($($all)*));
    }
}

const INTERNAL_CPP_STRUCTS: &'static str = r#"
/* THIS FILE IS GENERATED BY rust-cpp. DO NOT EDIT */

#include "stdint.h" // For {u}intN_t
#include <new> // For placement new
#include <cstdlib> // For abort

#if __cplusplus <= 199711L && !defined(_MSC_VER)
namespace rustcpp {
template<typename T>
struct is_default_constructible {
    template<typename X>
    static double test(int(*)[sizeof(new X)]);
    template<typename X>
    static char test(...);
    enum { value = sizeof(test<T>(0)) == sizeof(double) };
};
template<typename T>
struct is_copy_constructible {
    static T &declval();
    template<typename X>
    static double test(int(*)[sizeof(new X(declval()))]);
    template<typename X>
    static char test(...);
    enum { value = sizeof(test<T>(0)) == sizeof(double) };
};
template<bool B, class T = void> struct enable_if {};
template<class T> struct enable_if<true, T> { typedef T type; };
}
#else
#include <type_traits>
namespace rustcpp {
using std::is_default_constructible;
using std::is_copy_constructible;
using std::enable_if;
}
#endif

namespace rustcpp {
template<typename T>
typename enable_if<is_copy_constructible<T>::value>::type copy_helper(const void *src, void *dest)
{ new (dest) T (*static_cast<T const*>(src)); }
template<typename T>
typename enable_if<!is_copy_constructible<T>::value>::type copy_helper(const void *, void *)
{ std::abort(); }
template<typename T>
typename enable_if<is_default_constructible<T>::value>::type default_helper(void *dest)
{ new (dest) T(); }
template<typename T>
typename enable_if<!is_default_constructible<T>::value>::type default_helper(void *)
{ std::abort(); }
}

"#;

lazy_static! {
    static ref OUT_DIR: PathBuf =
        PathBuf::from(env::var("OUT_DIR").expect(r#"
-- rust-cpp fatal error --

The OUT_DIR environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#));

    static ref CPP_DIR: PathBuf = OUT_DIR.join("rust_cpp");

    static ref CARGO_MANIFEST_DIR: PathBuf =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect(r#"
-- rust-cpp fatal error --

The CARGO_MANIFEST_DIR environment variable was not set.
NOTE: rust-cpp's build function must be run in a build script."#));
}

fn gen_cpp_lib(visitor: &Handle) -> PathBuf {
    let result_path = CPP_DIR.join("cpp_closures.cpp");
    let mut output = File::create(&result_path).expect("Unable to generate temporary C++ file");

    write!(output, "{}", INTERNAL_CPP_STRUCTS).unwrap();

    write!(output, "{}\n\n", &visitor.snippets).unwrap();

    let mut sizealign = vec![];
    for &Closure { ref body, ref sig } in &visitor.closures {
        let &ClosureSig {
            ref captures,
            ref cpp,
            ..
        } = sig;

        let hash = sig.name_hash();
        let name = sig.extern_name();

        let is_void = cpp == "void";

        // Generate the sizes array with the sizes of each of the argument types
        if is_void {
            sizealign.push(format!(
                "{{{hash}ull, 0, 1, 0}}",
                hash = hash
            ));
        } else {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=cpp));
        }
        for &Capture { ref cpp, .. } in captures {
            sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=cpp));
        }

        // Generate the parameters and function declaration
        let params = captures
            .iter()
            .map(
                |&Capture {
                     mutable,
                     ref name,
                     ref cpp,
                 }| if mutable {
                    format!("{} & {}", cpp, name)
                } else {
                    format!("{} const& {}", cpp, name)
                },
            )
            .collect::<Vec<_>>()
            .join(", ");

        if is_void {
            write!(
                output,
                r#"
extern "C" {{
void {name}({params}) {{
{body}
}}
}}
"#,
                name = &name,
                params = params,
                body = body.node
            ).unwrap();
        } else {
            let comma = if params.is_empty() { "" } else { "," };
            let args = captures
                .iter()
                .map(|&Capture { ref name, .. }| name.as_ref())
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                output,
                r#"
static inline {ty} {name}_impl({params}) {{
{body}
}}
extern "C" {{
void {name}({params}{comma} void* __result) {{
    ::new(__result) ({ty})({name}_impl({args}));
}}
}}
"#,
                name = &name,
                params = params,
                comma = comma,
                ty = cpp,
                args = args,
                body = body.node
            ).unwrap();
        }
    }

    for class in &visitor.classes {
        let hash = class.name_hash();

        // Generate the sizes array
        sizealign.push(format!("{{
                {hash}ull,
                sizeof({type}),
                rustcpp::AlignOf<{type}>::value,
                rustcpp::Flags<{type}>::value
            }}", hash=hash, type=class.cpp));

        // Generate constructor
        write!(
            output,
            r#"
extern "C" {{
void __cpp_destructor_{hash}(void *ptr) {{
    typedef {cpp_name} T;
    static_cast<T*>(ptr)->~T();
}}
void __cpp_copy_{hash}(const void *src, void *dest) {{
    rustcpp::copy_helper<{cpp_name}>(src, dest);
}}
void __cpp_default_{hash}(void *dest) {{
    rustcpp::default_helper<{cpp_name}>(dest);
}}
}}
"#,
            hash = hash,
            cpp_name = class.cpp
        ).unwrap();
    }

    let mut magic = vec![];
    for mag in STRUCT_METADATA_MAGIC.iter() {
        magic.push(format!("{}", mag));
    }

    write!(
        output,
        r#"

namespace rustcpp {{

template<typename T>
struct AlignOf {{
    struct Inner {{
        char a;
        T b;
    }};
    static const uintptr_t value = sizeof(Inner) - sizeof(T);
}};

template<typename T>
struct Flags {{
    static const uintptr_t value =
        (is_copy_constructible<T>::value << {flag_is_copy_constructible}) |
        (is_default_constructible<T>::value << {flag_is_default_constructible}) |
#if __cplusplus > 199711L
        (std::is_trivially_destructible<T>::value << {flag_is_trivially_destructible}) |
        (std::is_trivially_copyable<T>::value << {flag_is_trivially_copyable}) |
        (std::is_trivially_default_constructible<T>::value << {flag_is_trivially_default_constructible}) |
#endif
        0;
}};


struct SizeAlign {{
    uint64_t hash;
    uint64_t size;
    uint64_t align;
    uint64_t flags;
}};

struct MetaData {{
    uint8_t magic[128];
    uint8_t version[16];
    uint64_t length;
    SizeAlign data[{length}];
}};

MetaData
#ifdef __GNUC__
    __attribute__((weak))
#endif
    metadata = {{
    {{ {magic} }},
    "{version}",
    {length},
    {{ {data} }}
}};

}} // namespace rustcpp
"#,
        data = sizealign.join(", "),
        length = sizealign.len(),
        magic = magic.join(", "),
        version = VERSION,
        flag_is_copy_constructible = flags::IS_COPY_CONSTRUCTIBLE,
        flag_is_default_constructible = flags::IS_DEFAULT_CONSTRUCTIBLE,
        flag_is_trivially_destructible = flags::IS_TRIVIALLY_DESTRUCTIBLE,
        flag_is_trivially_copyable = flags::IS_TRIVIALLY_COPYABLE,
        flag_is_trivially_default_constructible = flags::IS_TRIVIALLY_DEFAULT_CONSTRUCTIBLE,
    ).unwrap();

    result_path
}

fn clean_artifacts() {
    if CPP_DIR.is_dir() {
        remove_dir_all(&*CPP_DIR).expect(
            r#"
-- rust-cpp fatal error --

Failed to remove existing build artifacts from output directory."#,
        );
    }

    create_dir(&*CPP_DIR).expect(
        r#"
-- rust-cpp fatal error --

Failed to create output object directory."#,
    );
}

/// This struct is for advanced users of the build script. It allows providing
/// configuration options to `cpp` and the compiler when it is used to build.
///
/// ## API Note
///
/// Internally, `cpp` uses the `cc` crate to build the compilation artifact,
/// and many of the methods defined on this type directly proxy to an internal
/// `cc::Build` object.
pub struct Config {
    cc: cc::Build,
    std_flag_set: bool, // true if the -std flag was specified
}

impl Config {
    /// Create a new `Config` object. This object will hold the configuration
    /// options which control the build. If you don't need to make any changes,
    /// `cpp_build::build` is a wrapper function around this interface.
    pub fn new() -> Config {
        let mut cc = cc::Build::new();
        cc.cpp(true).include(&*CARGO_MANIFEST_DIR);
        Config { cc: cc, std_flag_set: false }
    }

    /// Add a directory to the `-I` or include path for headers
    pub fn include<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cc.include(dir);
        self
    }

    /// Specify a `-D` variable with an optional value
    pub fn define(&mut self, var: &str, val: Option<&str>) -> &mut Self {
        self.cc.define(var, val);
        self
    }

    // XXX: Make sure that this works with sizes logic
    /// Add an arbitrary object file to link in
    pub fn object<P: AsRef<Path>>(&mut self, obj: P) -> &mut Self {
        self.cc.object(obj);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler
    pub fn flag(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag(flag);
        self
    }

    /// Add an arbitrary flag to the invocation of the compiler if it supports it
    pub fn flag_if_supported(&mut self, flag: &str) -> &mut Self {
        if flag.starts_with("-std=") {
            self.std_flag_set = true;
        }
        self.cc.flag_if_supported(flag);
        self
    }

    // XXX: Make sure this works with sizes logic
    /// Add a file which will be compiled
    pub fn file<P: AsRef<Path>>(&mut self, p: P) -> &mut Self {
        self.cc.file(p);
        self
    }

    /// Set the standard library to link against when compiling with C++
    /// support.
    ///
    /// The default value of this property depends on the current target: On
    /// OS X `Some("c++")` is used, when compiling for a Visual Studio based
    /// target `None` is used and for other targets `Some("stdc++")` is used.
    ///
    /// A value of `None` indicates that no automatic linking should happen,
    /// otherwise cargo will link against the specified library.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_link_stdlib(&mut self, cpp_link_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_link_stdlib(cpp_link_stdlib);
        self
    }

    /// Force the C++ compiler to use the specified standard library.
    ///
    /// Setting this option will automatically set `cpp_link_stdlib` to the same
    /// value.
    ///
    /// The default value of this option is always `None`.
    ///
    /// This option has no effect when compiling for a Visual Studio based
    /// target.
    ///
    /// This option sets the `-stdlib` flag, which is only supported by some
    /// compilers (clang, icc) but not by others (gcc). The library will not
    /// detect which compiler is used, as such it is the responsibility of the
    /// caller to ensure that this option is only used in conjuction with a
    /// compiler which supports the `-stdlib` flag.
    ///
    /// A value of `None` indicates that no specific C++ standard library should
    /// be used, otherwise `-stdlib` is added to the compile invocation.
    ///
    /// The given library name must not contain the `lib` prefix.
    pub fn cpp_set_stdlib(&mut self, cpp_set_stdlib: Option<&str>) -> &mut Self {
        self.cc.cpp_set_stdlib(cpp_set_stdlib);
        self
    }

    // XXX: Add support for custom targets
    //
    // /// Configures the target this configuration will be compiling for.
    // ///
    // /// This option is automatically scraped from the `TARGET` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn target(&mut self, target: &str) -> &mut Self {
    //     self.cc.target(target);
    //     self
    // }

    /// Configures the host assumed by this configuration.
    ///
    /// This option is automatically scraped from the `HOST` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn host(&mut self, host: &str) -> &mut Self {
        self.cc.host(host);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level(&mut self, opt_level: u32) -> &mut Self {
        self.cc.opt_level(opt_level);
        self
    }

    /// Configures the optimization level of the generated object files.
    ///
    /// This option is automatically scraped from the `OPT_LEVEL` environment
    /// variable by build scripts, so it's not required to call this function.
    pub fn opt_level_str(&mut self, opt_level: &str) -> &mut Self {
        self.cc.opt_level_str(opt_level);
        self
    }

    /// Configures whether the compiler will emit debug information when
    /// generating object files.
    ///
    /// This option is automatically scraped from the `PROFILE` environment
    /// variable by build scripts (only enabled when the profile is "debug"), so
    /// it's not required to call this function.
    pub fn debug(&mut self, debug: bool) -> &mut Self {
        self.cc.debug(debug);
        self
    }

    // XXX: Add support for custom out_dir
    //
    // /// Configures the output directory where all object files and static
    // /// libraries will be located.
    // ///
    // /// This option is automatically scraped from the `OUT_DIR` environment
    // /// variable by build scripts, so it's not required to call this function.
    // pub fn out_dir<P: AsRef<Path>>(&mut self, out_dir: P) -> &mut Self {
    //     self.cc.out_dir(out_dir);
    //     self
    // }

    /// Configures the compiler to be used to produce output.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn compiler<P: AsRef<Path>>(&mut self, compiler: P) -> &mut Self {
        self.cc.compiler(compiler);
        self
    }

    /// Configures the tool used to assemble archives.
    ///
    /// This option is automatically determined from the target platform or a
    /// number of environment variables, so it's not required to call this
    /// function.
    pub fn archiver<P: AsRef<Path>>(&mut self, archiver: P) -> &mut Self {
        self.cc.archiver(archiver);
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Self {
        // XXX: Use this to control the cargo metadata which rust-cpp produces
        self.cc.cargo_metadata(cargo_metadata);
        self
    }

    /// Configures whether the compiler will emit position independent code.
    ///
    /// This option defaults to `false` for `i686` and `windows-gnu` targets and
    /// to `true` for all other targets.
    pub fn pic(&mut self, pic: bool) -> &mut Self {
        self.cc.pic(pic);
        self
    }

    /// Extracts `cpp` declarations from the passed-in crate root, and builds
    /// the associated static library to be linked in to the final binary.
    ///
    /// This method does not perform rust codegen - that is performed by `cpp`
    /// and `cpp_macros`, which perform the actual procedural macro expansion.
    ///
    /// This method may technically be called more than once for ergonomic
    /// reasons, but that usually won't do what you want. Use a different
    /// `Config` object each time you want to build a crate.
    pub fn build<P: AsRef<Path>>(&mut self, crate_root: P) {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            VERSION,
            "Internal Error: mismatched cpp_common and cpp_build versions"
        );

        // Clean up any leftover artifacts
        clean_artifacts();

        // Parse the crate
        let mut sm = SourceMap::new();
        let krate = match sm.add_crate_root(crate_root) {
            Ok(krate) => krate,
            Err(err) => {
                let mut err_s = err.to_string();
                if let Some(i) = err_s.find("unparsed tokens after") {
                    // Strip the long error message from syn
                    err_s = err_s[0..i].to_owned();
                }
                warnln!(
                    r#"-- rust-cpp parse error --
There was an error parsing the crate for the rust-cpp build script:
{}
In order to provide a better error message, the build script will exit successfully, such that rustc can provide an error message."#,
                    err_s
                );
                return;
            }
        };

        // Parse the macro definitions
        let mut visitor = Handle {
            closures: Vec::new(),
            classes: Vec::new(),
            snippets: String::new(),
            sm: &sm,
        };
        visitor.visit_crate(&krate);

        // Generate the C++ library code
        let filename = gen_cpp_lib(&visitor);

        // Ensure C++11 mode is enabled. We rely on some C++11 construct, so we
        // must enable C++11 by default.
        // MSVC, GCC >= 5, Clang >= 6 defaults to C++14, but since we want to
        // supports older compiler which defaults to C++98, we need to
        // explicitly set the "-std" flag.
        // Ideally should be done by https://github.com/alexcrichton/cc-rs/issues/191
        if !self.std_flag_set {
            self.cc.flag_if_supported("-std=c++11");
        }

        // Build the C++ library
        self.cc.file(filename).compile(LIB_NAME);
    }
}

/// Run the `cpp` build process on the crate with a root at the given path.
/// Intended to be used within `build.rs` files.
pub fn build<P: AsRef<Path>>(path: P) {
    Config::new().build(path)
}

struct Handle<'a> {
    closures: Vec<Closure>,
    classes: Vec<Class>,
    snippets: String,
    sm: &'a SourceMap,
}

fn extract_with_span(spanned: &mut Spanned<String>, src: &str, offset: usize, sm: &SourceMap) {
    if spanned.span != DUMMY_SPAN {
        let src_slice = &src[spanned.span.lo..spanned.span.hi];
        spanned.span.lo += offset;
        spanned.span.hi += offset;

        let loc = sm.locinfo(spanned.span).unwrap();
        spanned.node = format!("#line {} {:?}\n", loc.line, loc.path);
        for _ in 0..loc.col {
            spanned.node.push(' ');
        }
        spanned.node.push_str(src_slice);
    }
}

impl<'a> Visitor for Handle<'a> {
    fn visit_mac(&mut self, mac: &Mac) {
        if mac.path.segments.len() != 1 {
            return;
        }
        if mac.path.segments[0].ident.as_ref() == "cpp" {
            let tts = &mac.tts;
            assert!(tts.len() >= 1);
            let span = Span {
                lo: tts[0].span().lo,
                hi: tts[tts.len() - 1].span().hi,
            };
            let src = self.sm.source_text(span).unwrap();
            let input = synom::ParseState::new(&src);
            match parsing::build_macro(input).expect("cpp! macro") {
                Macro::Closure(mut c) => {
                    extract_with_span(&mut c.body, &src, span.lo, self.sm);
                    self.closures.push(c);
                }
                Macro::Lit(mut l) => {
                    extract_with_span(&mut l, &src, span.lo, self.sm);
                    self.snippets.push('\n');
                    self.snippets.push_str(&l.node);
                }
            }
        }
        if mac.path.segments[0].ident.as_ref() == "cpp_class" {
            let tts = &mac.tts;
            assert!(tts.len() >= 1);
            let span = Span {
                lo: tts[0].span().lo,
                hi: tts[tts.len() - 1].span().hi,
            };
            let src = self.sm.source_text(span).unwrap();
            let input = synom::ParseState::new(&src);
            let class = parsing::class_macro(input).expect("cpp_class! macro");
            self.classes.push(class);
        }
    }
}
