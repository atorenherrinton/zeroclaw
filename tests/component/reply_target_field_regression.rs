//! Regression guard for ChannelMessage field naming consistency.
//! This test prevents accidental reintroduction of the removed `reply_to` field
//! in Rust source code where `reply_target` must be used.

use std::fs;
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

const SCAN_PATHS: &[&str] = &[
    "src",
    "crates/zeroclaw-api/src",
    "crates/zeroclaw-channels/src",
];

// Field spelling belongs to its type. DeliveryConfig.reply_to remains a valid
// configuration field. Check ChannelMessage declarations and constructions in
// the syntax tree; cargo check resolves field accesses (including aliases).
#[derive(Default)]
struct LegacyChannelFields {
    lines: Vec<usize>,
}

impl<'ast> Visit<'ast> for LegacyChannelFields {
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if item.ident == "ChannelMessage" {
            for field in &item.fields {
                if field.ident.as_ref().is_some_and(|name| name == "reply_to") {
                    self.lines.push(field.span().start().line);
                }
            }
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_expr_struct(&mut self, item: &'ast syn::ExprStruct) {
        if item
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "ChannelMessage")
        {
            for field in &item.fields {
                if matches!(&field.member, syn::Member::Named(name) if name == "reply_to") {
                    self.lines.push(field.span().start().line);
                }
            }
        }
        visit::visit_expr_struct(self, item);
    }
}

fn legacy_channel_fields(source: &str) -> Vec<usize> {
    let file = syn::parse_file(source).expect("source must parse for field policy scan");
    let mut detector = LegacyChannelFields::default();
    detector.visit_file(&file);
    detector.lines
}

#[test]
fn channel_field_guard_distinguishes_configuration_and_ignores_comments() {
    assert!(
        legacy_channel_fields(
            r#"
        struct DeliveryConfig { reply_to: Option<String> }
        fn config() { let _ = DeliveryConfig { reply_to: None }; }
        // ChannelMessage { reply_to: None }
        const HELP: &str = "reply_to: is a config field";
    "#
        )
        .is_empty()
    );
    assert_eq!(
        legacy_channel_fields(
            r#"
        struct ChannelMessage { reply_to: String }
        fn message() { let _ = api::ChannelMessage { reply_to: String::new() }; }
    "#
        )
        .len(),
        2
    );
    assert!(
        legacy_channel_fields(
            r#"
        struct ChannelMessage { reply_target: String }
        fn message() { let _ = ChannelMessage { reply_target: String::new() }; }
    "#
        )
        .is_empty()
    );
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("Failed to read directory {}: {err}", dir.display()));

    for entry in entries {
        let entry =
            entry.unwrap_or_else(|err| panic!("Failed to read entry in {}: {err}", dir.display()));
        let path = entry.path();

        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn source_does_not_use_legacy_reply_to_field() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rust_files = Vec::new();

    for relative in SCAN_PATHS {
        collect_rs_files(&root.join(relative), &mut rust_files);
    }

    rust_files.sort();

    let mut violations = Vec::new();

    for file_path in rust_files {
        let content = fs::read_to_string(&file_path).unwrap_or_else(|err| {
            panic!("Failed to read source file {}: {err}", file_path.display())
        });

        for line in legacy_channel_fields(&content) {
            let relative = file_path.strip_prefix(root).unwrap_or(&file_path);
            violations.push(format!(
                "{}:{line} uses ChannelMessage.reply_to",
                relative.display()
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Found legacy `reply_to` field usage:\n{}",
        violations.join("\n")
    );
}
