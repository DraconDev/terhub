use terhub::agent::{builtin_manifests, classify_screen_owned, CompiledManifest};

#[test]
fn probe_muse_captures() {
    let m = CompiledManifest::compile_all(&builtin_manifests()).expect("compile");
    for p in ["%800", "%2857", "%32", "%2379", "%2806", "%2856"] {
        let path = format!("/tmp/muse-caps/muse-cap-{p}.txt");
        let Ok(screen) = std::fs::read_to_string(&path) else { continue };
        let hit = classify_screen_owned(&m, "muse-bin-1.4.14-R7309.1", &screen, false, false, 48);
        println!("{p}: {:?}", hit.map(|h| (h.state, h.summary, h.phase)));
    }
}
