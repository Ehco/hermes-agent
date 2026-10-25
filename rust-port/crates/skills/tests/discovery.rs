//! End-to-end tests: discovery ordering, category extraction, name lookup,
//! linked-files listing, and the real-fixture parse, mirroring
//! `tools/skills_tool.py` (find_all_skills / sort_skills / skill_view).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use hermes_skills::{discover_skills, load_skill, locate_skill, SkillMeta};

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermes-skills-disc-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn discovers_real_fixtures_with_categories_and_deterministic_order() {
    let root = tmp_root("fixtures");
    write(
        &root,
        "apple/apple-notes/SKILL.md",
        &fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/apple-notes-SKILL.md"
        ))
        .unwrap(),
    );
    write(
        &root,
        "media/gif-search/SKILL.md",
        &fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gif-search-SKILL.md"
        ))
        .unwrap(),
    );
    let skills = discover_skills(&root);
    assert_eq!(skills.len(), 2);
    // sorted by (category, name): apple < media
    assert_eq!(skills[0].name, "apple-notes");
    assert_eq!(skills[0].category.as_deref(), Some("apple"));
    assert_eq!(skills[0].description, "Manage Apple Notes via memo CLI: create, search, edit.");
    assert_eq!(skills[1].name, "gif-search");
    assert_eq!(skills[1].category.as_deref(), Some("media"));
    assert!(skills[0].path.ends_with("apple-notes/SKILL.md"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn discovery_excludes_support_and_excluded_dirs_and_falls_back() {
    let root = tmp_root("walk");
    // category/skill with NO frontmatter name -> dir-name fallback and
    // first-non-heading-body-line description fallback (description omitted).
    write(&root, "tools/my-tool/SKILL.md", "---\n---\n\n# Heading\nFirst useful line.\n");
    // flat skill (root-level dir) -> category None
    write(&root, "flat-one/SKILL.md", "---\nname: flat-one\ndescription: flat\n---\n\nbody\n");
    // inside a skill dir: support dirs are NOT scanned as skills
    write(&root, "tools/my-tool/references/nested/SKILL.md", "---\nname: nested-skill\n---\n\nbody\n");
    // excluded dirs are never walked
    write(&root, "tools/node_modules/hidden/SKILL.md", "---\nname: hidden-skill\n---\n\nbody\n");

    let skills = discover_skills(&root);
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"flat-one"));
    assert!(names.contains(&"my-tool"));
    assert!(!names.contains(&"nested-skill"), "support dirs are not scanned as skills");
    assert!(!names.contains(&"hidden-skill"), "excluded dirs are never walked");

    let my_tool = skills.iter().find(|s| s.name == "my-tool").unwrap();
    assert_eq!(my_tool.category.as_deref(), Some("tools"));
    assert_eq!(my_tool.description, "First useful line.");
    let flat = skills.iter().find(|s| s.name == "flat-one").unwrap();
    assert_eq!(flat.category, None);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn lookup_works_by_frontmatter_name_and_load_returns_linked_files() {
    let root = tmp_root("lookup");
    write(&root, "tools/my-tool/SKILL.md", "---\nname: declared-name\ndescription: d\n---\n\n# Body\n");
    write(&root, "tools/my-tool/references/api.md", "api text");
    write(&root, "tools/my-tool/references/extra.txt", "wrong ext");
    write(
        &root,
        "tools/my-tool/templates/gen.py",
        "# template with a .py extension\n",
    );
    write(&root, "tools/my-tool/templates/sub/inner.yaml", "k: v\n");
    write(&root, "tools/my-tool/assets/logo.png", "binary-ish");
    write(&root, "tools/my-tool/assets/notes.txt", "not in scripts globs");
    write(&root, "tools/my-tool/scripts/run.sh", "#!/bin/sh\n");
    write(&root, "tools/my-tool/scripts/run.ts", "// ts\n");
    write(&root, "tools/my-tool/other.md", "top-level stray file");

    // by frontmatter name
    let found = locate_skill(&root, "declared-name").unwrap();
    assert!(found.ends_with("my-tool/SKILL.md"));
    // by directory name too
    assert!(locate_skill(&root, "my-tool").is_some());
    assert!(locate_skill(&root, "missing").is_none());

    let (content, files) = load_skill(&root, "declared-name").unwrap();
    assert!(content.contains("# Body"));
    let files: BTreeMap<String, Vec<String>> = files;
    assert_eq!(
        files.get("references").map(Vec::as_slice),
        Some(&["references/api.md".to_string()][..])
    );
    // templates globs: .py yes, recursive; .yaml nested yes; nothing else here
    assert_eq!(
        files.get("templates").map(Vec::as_slice),
        Some(
            &[
                "templates/gen.py".to_string(),
                "templates/sub/inner.yaml".to_string(),
            ][..]
        )
    );
    // assets: everything, files only, recursive — relative posix paths
    assert_eq!(
        files.get("assets").map(Vec::as_slice),
        Some(
            &[
                "assets/logo.png".to_string(),
                "assets/notes.txt".to_string(),
            ][..]
        )
    );
    // scripts: non-recursive *.sh/*.ts
    assert_eq!(
        files.get("scripts").map(Vec::as_slice),
        Some(
            &[
                "scripts/run.sh".to_string(),
                "scripts/run.ts".to_string(),
            ][..]
        )
    );
    // no other groups (references/extra.txt and other.md don't match any glob)
    assert_eq!(files.len(), 4);

    // empty groups are dropped: a skill with only a references/ dir
    write(&root, "only-refs/SKILL.md", "---\nname: only-refs\n---\n\nbody\n");
    write(&root, "only-refs/references/r.md", "x");
    let (_, files) = load_skill(&root, "only-refs").unwrap();
    let files: BTreeMap<String, Vec<String>> = files;
    assert_eq!(files.len(), 1);
    assert!(files.contains_key("references"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn linked_files_on_empty_skill_dir_is_empty() {
    let root = tmp_root("empty");
    write(&root, "bare/SKILL.md", "---\nname: bare\n---\n\nbody\n");
    let (_, files) = load_skill(&root, "bare").unwrap();
    assert!(files.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn skill_meta_is_sorted_by_category_then_name() {
    let mk = |cat: Option<&str>, name: &str| SkillMeta {
        name: name.to_string(),
        description: String::new(),
        category: cat.map(str::to_string),
        path: PathBuf::from(name),
    };
    let sorted = hermes_skills::sort_skills(vec![
        mk(Some("b"), "zeta"),
        mk(None, "aaa"),
        mk(Some("b"), "alpha"),
        mk(Some("a"), "x"),
    ]);
    let order: Vec<(&Option<String>, &str)> =
        sorted.iter().map(|s| (&s.category, s.name.as_str())).collect();
    assert_eq!(
        order,
        vec![
            (&None, "aaa"),
            (&Some("a".to_string()), "x"),
            (&Some("b".to_string()), "alpha"),
            (&Some("b".to_string()), "zeta"),
        ]
    );
}
