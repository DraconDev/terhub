use terhub::agent::{builtin_manifests, classify_screen, compile_all};

fn main() {
    let screen = std::fs::read_to_string("/tmp/gemini_quota_screen.txt").unwrap();
    let manifests = compile_all(&builtin_manifests()).unwrap();
    let hit = classify_screen(&manifests, "agy", &screen, false, false);
    println!("{:?}", hit);
}
