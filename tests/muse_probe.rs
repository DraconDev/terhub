use terhub::agent::{builtin_manifests, classify_screen_owned};

#[test]
fn probe() {
    let m = terhub::agent::compile_all(&builtin_manifests()).expect("compile");
    for p in ["%2900", "%2901", "%2902"] {
        let Ok(s) = std::fs::read_to_string(format!("/tmp/muse-caps2/p{p}.txt")) else { continue };
        println!("{p}: {:?}", classify_screen_owned(&m, "muse-bin-1.4.14-R7309.1", s.as_str(), false, false, 48).map(|h| (h.state, h.summary, h.phase)));
    }
}
