use slug::slugify;

#[test]
fn hidden_contract() {
    assert_eq!(slugify("Hello, World!"), "hello-world");
    assert_eq!(slugify("  many   spaces  "), "many-spaces");
    assert_eq!(slugify("Rust 2024 edition"), "rust-2024-edition");
    assert_eq!(slugify("--already--slugged--"), "already-slugged");
    assert_eq!(slugify("Crème brûlée"), "crme-brle");
    assert_eq!(slugify("!!!"), "");
    assert_eq!(slugify(""), "");
    assert_eq!(slugify("MiXeD Case"), "mixed-case");
}
