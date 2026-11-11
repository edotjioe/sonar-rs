// Embeds static/ (the mixer UI) into the binary: a table of (path, bytes) in $OUT_DIR/static_files.rs.
use std::path::Path;

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, root, out);
        } else {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push(format!("    ({rel:?}, include_bytes!({:?})),", p.canonicalize().unwrap()));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=static");
    let root = Path::new("static");
    let mut files = vec![];
    walk(root, root, &mut files);
    let code = format!("pub static FILES: &[(&str, &[u8])] = &[\n{}\n];\n", files.join("\n"));
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("static_files.rs"), code).unwrap();
}
