fn main() {
    let src_dir = std::path::Path::new("src");

    let mut build = cc::Build::new();
    build.include(src_dir).file(src_dir.join("parser.c"));

    // scanner.c is optional; include if present.
    let scanner = src_dir.join("scanner.c");
    if scanner.exists() {
        build.file(&scanner);
    }

    build.compile("tree_sitter_liquid");
}
