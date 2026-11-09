use terhub::agent::{builtin_manifests, classify_screen_owned};

#[test]
fn probe() {
    let m = terhub::agent::compile_all(&builtin_manifests()).expect("compile");
    let muse = m.iter().find(|x| x.agent == "muse").expect("muse manifest");
    println!("commands={:?}", muse.commands);
    for p in ["%2884", "%2885", "%2886"] {
        let s = std::fs::read_to_string(format!("/tmp/muse-caps2/p{p}.txt")).unwrap();
        println!("{p}: lines={} muse.classify={:?}", s.lines().count(), muse.classify(&s, 48));
    }
}
