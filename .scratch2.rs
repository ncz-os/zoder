    // Cross-chunk context: names that exist in the post-change files but
    // outside the chunk (bindings earlier in the enclosing function, imports,
    // top-level definitions). Working-tree reviews read the files on disk;
    // branch reviews read them at HEAD. `ZODER_REVIEW_CONTEXT=0` disables.
    //
    // The reads go through ONE per-review cache (D10.7): this header, the
    // false-undefined filter below and the confirm layer's grounding all ask
    // about the same files, and each of them used to re-read (or re-run
    // `git show` for) a file per chunk and per finding.
    let post_image_source = |path: &str| -> Option<String> {
        if label.starts_with("working-tree") {
            std::fs::read_to_string(root.join(path)).ok()
        } else {
            run_git(&cwd, &["show", &format!("HEAD:{path}")]).ok()
        }
    };
    let post_images = Rc::new(PostImageCache::new());
    let read_post_image = post_images.reader(post_image_source);
