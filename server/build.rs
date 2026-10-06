// Embeds every file under ../web (the PWA) and ../mod (the Claude Code plugin `sheprd setup` installs)
// into the binary as (path, bytes) pairs.
use std::path::Path;

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "node_modules" || n == "tests") {
                continue;
            }
            walk(&p, root, out);
        } else {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((rel, p.canonicalize().unwrap().to_string_lossy().to_string()));
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
}

fn table(name: &str, dir: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    files.sort();
    let mut code = format!("pub static {name}: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in files {
        code.push_str(&format!("    ({rel:?}, include_bytes!({abs:?})),\n"));
    }
    code.push_str("];\n");
    code
}

fn main() {
    let code = table("ASSETS", "../web") + &table("MOD", "../mod");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("assets.rs"), code).unwrap();
}
