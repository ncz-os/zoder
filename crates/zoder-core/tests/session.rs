use zoder_core::Session;

#[test]
#[allow(deprecated)] // exercising the bare save() path in a single-process test fixture
fn save_load_latest_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let mut a = Session::load_or_new(d, "alpha").unwrap();
    a.push("user", "hello");
    a.push("assistant", "hi");
    a.save(d).unwrap();

    // sleep boundary not needed: bravo saved later gets a >= updated stamp.
    let mut b = Session::load_or_new(d, "bravo").unwrap();
    b.push("user", "second");
    b.save(d).unwrap();

    let reloaded = Session::load_or_new(d, "alpha").unwrap();
    assert_eq!(reloaded.messages.len(), 2);
    assert_eq!(reloaded.messages[0].content, "hello");

    let latest = Session::latest(d).unwrap().unwrap();
    assert!(latest.updated >= reloaded.updated);

    let list = Session::list(d).unwrap();
    assert_eq!(list.len(), 2);
}

#[test]
#[allow(deprecated)] // exercising the bare save() path in a single-process test fixture
fn id_is_path_safe() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut s = Session::load_or_new(d, "../../etc/passwd").unwrap();
    s.push("user", "x");
    s.save(d).unwrap();
    // The traversal characters are sanitized, so the file stays inside `d`.
    let entries: Vec<_> = std::fs::read_dir(d)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);
    let name = entries[0].file_name().into_string().unwrap();
    assert!(!name.contains('/'));
}
