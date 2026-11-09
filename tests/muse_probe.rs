use terhub::agent::{builtin_manifests, classify_screen_owned};

#[test]
fn probe_muse() {
    let m = terhub::agent::compile_all(&builtin_manifests()).expect("compile");
    let cases = [
        ("%32 picker", "/tmp/muse-caps/muse-cap-%32-picker.txt"),
        ("%800 at rest", "/tmp/muse-caps/muse-cap-%800.txt"),
        ("%2806 watcher", "/tmp/muse-caps/muse-cap-%2806.txt"),
    ];
    for (name, path) in cases {
        let Ok(screen) = std::fs::read_to_string(path) else { continue };
        let hit = classify_screen_owned(&m, "muse-bin-1.4.14-R7309.1", &screen, false, false, 48);
        println!("{name}: {:?}", hit.map(|h| (h.state, h.summary)));
    }
}
