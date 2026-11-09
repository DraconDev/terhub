use terhub::agent::{builtin_manifests, classify_screen_owned};

#[test]
fn probe() {
    let m = terhub::agent::compile_all(&builtin_manifests()).expect("compile");
    for p in ["%2884", "%2885", "%2886"] {
        let s = std::fs::read_to_string(format!("/tmp/muse-caps2/p{p}.txt")).unwrap();
        println!("{p}: {:?}", classify_screen_owned(&m, "muse-bin-1.4.14-R7309.1", &s, false, false, 48).map(|h| (h.state, h.summary)));
        println!("{p}: visible=0 -> {:?}", classify_screen_owned(&m, "muse-bin-1.4.14-R7309.1", &s, false, false, 0).map(|h| (h.state, h.summary)));
    }
}
