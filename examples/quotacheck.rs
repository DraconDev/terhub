use terhub::agent::{builtin_manifests, classify_screen, compile_all};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let cmd = std::env::args().nth(2).unwrap_or_else(|| "agy".to_string());
    let screen = std::fs::read_to_string(path).unwrap();
    let manifests = compile_all(&builtin_manifests()).unwrap();
    let hit = classify_screen(&manifests, &cmd, &screen, false, false);
    println!("{hit:?}");
}
