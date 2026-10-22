fn main() {
    let screen = std::fs::read_to_string("/tmp/screen26.txt").unwrap();
    let manifests = terhub::agent::builtin_manifests();
    let compiled = terhub::agent::compile_all_lossy(&manifests).0;
    let hit = terhub::agent::classify_screen(&compiled, "minimax-code", &screen, false, false);
    println!("hit = {hit:?}");
    let m = compiled.iter().find(|m| m.agent == "minimax-code").unwrap();
    println!("classify = {:?}", m.classify(&screen));
}
