use zoder_core::Corpus;

fn write_corpus(dir: &std::path::Path, json: &str) -> std::path::PathBuf {
    let p = dir.join("model_corpus.json");
    std::fs::write(&p, json).unwrap();
    p
}

#[test]
fn lenient_load_skips_invalid_entries() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_corpus(
        dir.path(),
        r#"{"models":[
            {"id":"good/model","free":true,"route_candidate":true,"kind":"chat"},
            {"no_id":"bad"},
            {"id":""}
        ]}"#,
    );
    let c = Corpus::load(&p).unwrap();
    assert_eq!(c.models.len(), 1);
    assert_eq!(c.models[0].id, "good/model");
    assert_eq!(c.free_chat().count(), 1);
}

#[test]
fn reconcile_adds_retires_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_corpus(
        dir.path(),
        r#"{"models":[
            {"id":"keep/model","free":true,"route_candidate":true,"kind":"chat"},
            {"id":"gone/model","free":true,"route_candidate":true,"kind":"chat"}
        ]}"#,
    );
    let mut c = Corpus::load(&p).unwrap();
    let served = vec!["keep/model".to_string(), "brand/new".to_string()];
    let rep = c.reconcile(&served);

    assert_eq!(rep.added, vec!["brand/new".to_string()]);
    assert_eq!(rep.retired, vec!["gone/model".to_string()]);
    assert_eq!(rep.kept, 1);

    // New model is present but NOT routable (conservative).
    let nu = c.get("brand/new").unwrap();
    assert!(!nu.routable());
    assert!(!nu.free);

    // Retired model is no longer a route candidate.
    let gone = c.get("gone/model").unwrap();
    assert!(!gone.route_candidate);

    // Persist round-trips.
    c.save(&p).unwrap();
    let reloaded = Corpus::load(&p).unwrap();
    assert!(reloaded.get("brand/new").is_some());
}
