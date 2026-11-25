use proc_macro2::{Delimiter, TokenStream, TokenTree};
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{ImDocument, Item, TableLike};

const FORBIDDEN_CRATE_FAMILIES: &[&str] = &[
    "sentry",
    "posthog",
    "segment",
    "mixpanel",
    "amplitude",
    "datadog",
    "axiom",
    "honeycomb",
    "bugsnag",
    "rollbar",
    "opentelemetry-exporter",
    "opentelemetry-otlp",
    "opentelemetry-jaeger",
    "opentelemetry-prometheus",
    "opentelemetry-zipkin",
    "opentelemetry-stdout",
    "opentelemetry-datadog",
    "opentelemetry-application-insights",
    "opentelemetry-stackdriver",
];

// Join host labels at runtime so the host scan also covers this test file.
const FORBIDDEN_HOST_LABELS: &[(&str, &str)] = &[
    ("posthog", "com"),
    ("sentry", "io"),
    ("segment", "io"),
    ("mixpanel", "com"),
    ("amplitude", "com"),
    ("axiom", "co"),
    ("honeycomb", "io"),
    ("bugsnag", "com"),
    ("rollbar", "com"),
    ("t3", "codes"),
];

fn visit_files(dir: &Path, visit: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display())) {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            visit_files(&entry.path(), visit);
        } else {
            visit(&entry.path());
        }
    }
}

fn forbidden_crate(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('_', "-");
    FORBIDDEN_CRATE_FAMILIES.iter().copied().any(|family| {
        normalized == family
            || normalized
                .strip_prefix(family)
                .is_some_and(|suffix| suffix.starts_with('-'))
    }) || normalized
        .strip_prefix("opentelemetry-")
        .is_some_and(|suffix| suffix.split('-').any(|part| part == "exporter"))
}

fn check_crate(
    name: &str,
    span: Option<std::ops::Range<usize>>,
    text: &str,
    path: &Path,
    findings: &mut Vec<String>,
) {
    if forbidden_crate(name) {
        let line = span.map_or(1, |span| text[..span.start].split('\n').count());
        findings.push(format!("{}:{line}: forbidden crate {name}", path.display()));
    }
}

fn check_dependencies(
    dependencies: &dyn TableLike,
    text: &str,
    path: &Path,
    findings: &mut Vec<String>,
) {
    for (alias, entry) in dependencies.iter() {
        let renamed = entry.as_table_like().and_then(|entry| entry.get("package"));
        if let Some((package, name)) =
            renamed.and_then(|item| item.as_str().map(|name| (item, name)))
        {
            check_crate(name, package.span(), text, path, findings);
        } else {
            let span = dependencies.key(alias).and_then(toml_edit::Key::span);
            let name = alias.split_once(':').map_or(alias, |(name, _)| name);
            check_crate(name, span, text, path, findings);
        }
    }
}

fn check_manifest(table: &dyn TableLike, text: &str, path: &Path, findings: &mut Vec<String>) {
    for (key, item) in table.iter() {
        let Some(nested) = item.as_table_like() else {
            continue;
        };
        match key {
            "dependencies" | "dev-dependencies" | "build-dependencies" | "dev_dependencies"
            | "build_dependencies" | "replace" => {
                check_dependencies(nested, text, path, findings);
            }
            "patch" => {
                for (_, registry) in nested.iter() {
                    if let Some(dependencies) = registry.as_table_like() {
                        check_dependencies(dependencies, text, path, findings);
                    }
                }
            }
            "target" => {
                for (_, target) in nested.iter() {
                    if let Some(target) = target.as_table_like() {
                        check_manifest(target, text, path, findings);
                    }
                }
            }
            "workspace" => check_manifest(nested, text, path, findings),
            _ => {}
        }
    }
}

fn check_manifests(root: &Path, findings: &mut Vec<String>) {
    let lock = root.join("Cargo.lock");
    let text = read(&lock);
    let doc = ImDocument::parse(text.as_str()).expect("parse Cargo.lock");
    let packages = doc
        .get("package")
        .and_then(Item::as_array_of_tables)
        .expect("Cargo.lock package table");
    for package in packages.iter() {
        let name = package.get("name").expect("locked package name");
        check_crate(
            name.as_str().unwrap(),
            name.span(),
            &text,
            Path::new("Cargo.lock"),
            findings,
        );
    }

    let mut check = |path: &Path| {
        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
            let display_path = path.strip_prefix(root).unwrap();
            let text = read(path);
            let doc = ImDocument::parse(text.as_str())
                .unwrap_or_else(|err| panic!("{}: {err}", display_path.display()));
            check_manifest(doc.as_table(), &text, display_path, findings);
            let name = doc
                .get("package")
                .and_then(Item::as_table_like)
                .and_then(|package| package.get("name"));
            if let Some((item, name)) = name.and_then(|item| item.as_str().map(|name| (item, name)))
            {
                check_crate(name, item.span(), &text, display_path, findings);
            }
        }
    };
    check(&root.join("Cargo.toml"));
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        // Cargo's output and Git metadata are outside the source tree.
        let name = entry.file_name();
        if entry.file_type().unwrap().is_dir() && name != "target" && name != ".git" {
            visit_files(&entry.path(), &mut check);
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn allowlist() -> Vec<(&'static str, bool)> {
    include_str!("allowed-urls.txt")
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (entry, reason) = line.split_once(" # ").unwrap_or_else(|| {
                panic!(
                    "allowed-urls.txt:{}: give a short reason after #",
                    index + 1
                )
            });
            let prefix = entry.starts_with("prefix ");
            let url = entry.strip_prefix("prefix ").unwrap_or(entry).trim();
            let address = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"));
            assert!(
                !reason.trim().is_empty()
                    && address.is_some_and(|address| {
                        !address.contains('*') && (!prefix || address.contains('.'))
                    }),
                "allowed-urls.txt:{}: use an exact URL or a concrete URL prefix and a reason",
                index + 1
            );
            Some((url, prefix))
        })
        .collect()
}

fn ident(token: &TokenTree, expected: &str) -> bool {
    matches!(token, TokenTree::Ident(name) if name == expected)
}

fn cfg_test(attribute: &proc_macro2::Group) -> bool {
    let tokens: Vec<_> = attribute.stream().into_iter().collect();
    match tokens.as_slice() {
        [name, TokenTree::Group(args)]
            if ident(name, "cfg") && args.delimiter() == Delimiter::Parenthesis =>
        {
            let mut args = args.stream().into_iter();
            args.next().is_some_and(|arg| ident(&arg, "test")) && args.next().is_none()
        }
        _ => false,
    }
}

fn path_attribute(attribute: &proc_macro2::Group) -> Option<PathBuf> {
    let tokens: Vec<_> = attribute.stream().into_iter().collect();
    match tokens.as_slice() {
        [name, TokenTree::Punct(eq), value] if ident(name, "path") && eq.as_char() == '=' => {
            let value: syn::LitStr = syn::parse2(std::iter::once(value.clone()).collect()).ok()?;
            Some(PathBuf::from(value.value()))
        }
        _ => None,
    }
}

// The token tree keeps raw strings and nested blocks intact. Exclude the
// module item attached to an exact #[cfg(test)] attribute. For an external
// module, also record its #[path] or its conventional Rust module path.
fn test_module_len(
    tokens: &[TokenTree],
    path_base: &Path,
    module_dir: &Path,
    test_files: &mut HashSet<PathBuf>,
) -> Option<usize> {
    let mut rest = tokens;
    let mut test_only = false;
    let mut explicit_path = None;
    while let [
        TokenTree::Punct(hash),
        TokenTree::Group(attribute),
        tail @ ..,
    ] = rest
    {
        if hash.as_char() != '#' || attribute.delimiter() != Delimiter::Bracket {
            break;
        }
        test_only |= cfg_test(attribute);
        if let Some(path) = path_attribute(attribute) {
            explicit_path = Some(path);
        }
        rest = tail;
    }
    if !test_only {
        return None;
    }
    if let [visibility, tail @ ..] = rest
        && ident(visibility, "pub")
    {
        rest = tail;
        if let [TokenTree::Group(scope), tail @ ..] = rest
            && scope.delimiter() == Delimiter::Parenthesis
        {
            rest = tail;
        }
    }
    match rest {
        [
            keyword,
            TokenTree::Ident(_),
            TokenTree::Group(body),
            tail @ ..,
        ] if ident(keyword, "mod") && body.delimiter() == Delimiter::Brace => {
            Some(tokens.len() - tail.len())
        }
        [
            keyword,
            TokenTree::Ident(name),
            TokenTree::Punct(end),
            tail @ ..,
        ] if ident(keyword, "mod") && end.as_char() == ';' => {
            let name = name.to_string();
            let paths = if let Some(path) = explicit_path {
                vec![path_base.join(path)]
            } else {
                vec![
                    module_dir.join(format!("{name}.rs")),
                    module_dir.join(name).join("mod.rs"),
                ]
            };
            test_files.extend(
                paths
                    .into_iter()
                    .filter_map(|path| path.canonicalize().ok()),
            );
            Some(tokens.len() - tail.len())
        }
        _ => None,
    }
}

fn module_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap();
    match path.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(path.file_stem().unwrap()),
    }
}

fn url_literals(
    stream: TokenStream,
    path_base: &Path,
    module_dir: &Path,
    test_files: &mut HashSet<PathBuf>,
    visit: &mut impl FnMut(usize, &str),
) {
    let tokens: Vec<_> = stream.into_iter().collect();
    let mut rest = tokens.as_slice();
    while let [first, tail @ ..] = rest {
        if let Some(len) = test_module_len(rest, path_base, module_dir, test_files) {
            rest = &rest[len..];
            continue;
        }
        if let [
            keyword,
            TokenTree::Ident(name),
            TokenTree::Group(body),
            after @ ..,
        ] = rest
            && ident(keyword, "mod")
            && body.delimiter() == Delimiter::Brace
        {
            let nested = module_dir.join(name.to_string());
            url_literals(body.stream(), &nested, &nested, test_files, visit);
            rest = after;
            continue;
        }
        match first {
            TokenTree::Group(group) => {
                url_literals(group.stream(), path_base, module_dir, test_files, visit);
            }
            TokenTree::Literal(literal) => {
                let span = literal.span();
                // Doc comments arrive as synthetic string tokens. Review
                // written string literals, including explicit doc attributes.
                if span
                    .source_text()
                    .is_some_and(|source| source.starts_with("//") || source.starts_with("/*"))
                {
                    rest = tail;
                    continue;
                }
                let line = span.start().line;
                match syn::parse2(std::iter::once(first.clone()).collect()) {
                    Ok(syn::Lit::Str(value)) => visit(line, &value.value()),
                    Ok(syn::Lit::ByteStr(value)) => {
                        visit(line, &String::from_utf8_lossy(&value.value()));
                    }
                    Ok(syn::Lit::CStr(value)) => {
                        visit(line, &String::from_utf8_lossy(value.value().as_bytes()));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        rest = tail;
    }
}

#[test]
fn workspace_has_no_telemetry_dependencies_hosts_or_unreviewed_urls() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.parent().unwrap().parent().unwrap();
    let mut findings = Vec::new();
    check_manifests(root, &mut findings);

    let hosts: Vec<_> = FORBIDDEN_HOST_LABELS
        .iter()
        .map(|(host, tld)| format!("{host}.{tld}"))
        .chain(std::iter::once(["datadog", "hq"].concat()))
        .collect();
    let allowlist = allowlist();
    let url = Regex::new(r#"(?i)https?://[^\s"'<>]*"#).unwrap();
    let mut test_files = HashSet::new();
    let mut unapproved_urls = Vec::new();
    for crate_entry in fs::read_dir(root.join("crates")).unwrap() {
        let crate_dir = crate_entry.unwrap().path();
        for area in ["src", "tests"] {
            let dir = crate_dir.join(area);
            if !dir.is_dir() {
                continue;
            }
            visit_files(&dir, &mut |path| {
                let relative = path.strip_prefix(root).unwrap();
                let raw =
                    fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", relative.display()));
                let text = String::from_utf8_lossy(&raw);
                for (index, line) in text.lines().enumerate() {
                    let line = line.to_ascii_lowercase();
                    for host in &hosts {
                        if line.contains(host) {
                            findings.push(format!(
                                "{}:{}: forbidden host {host}",
                                relative.display(),
                                index + 1
                            ));
                        }
                    }
                }
                let name = path.file_name().unwrap().to_string_lossy();
                // Files named *tests.rs are excluded only from URL review.
                if area == "src" && name.ends_with(".rs") && !name.ends_with("tests.rs") {
                    let tokens = text
                        .parse::<TokenStream>()
                        .unwrap_or_else(|err| panic!("{}: {err}", relative.display()));
                    url_literals(
                        tokens,
                        path.parent().unwrap(),
                        &module_dir(path),
                        &mut test_files,
                        &mut |line, literal| {
                            for candidate in url.find_iter(literal).map(|url| url.as_str()) {
                                if !allowlist.iter().any(|&(entry, prefix)| {
                                    if prefix {
                                        candidate.starts_with(entry)
                                    } else {
                                        candidate == entry
                                    }
                                }) {
                                    unapproved_urls.push((
                                        path.to_path_buf(),
                                        line,
                                        candidate.to_owned(),
                                    ));
                                }
                            }
                        },
                    );
                }
            });
        }
    }
    for (path, line, candidate) in unapproved_urls {
        if !test_files.contains(&path.canonicalize().unwrap()) {
            let relative = path.strip_prefix(root).unwrap();
            findings.push(format!(
                "{}:{line}: URL needs review: {candidate}",
                relative.display()
            ));
        }
    }
    findings.sort_unstable();
    findings.dedup();
    assert!(
        findings.is_empty(),
        "{}\nRemove forbidden crates/hosts. Remove an unapproved URL or add it to crates/sa-store/tests/allowed-urls.txt with a reason in review.",
        findings.join("\n")
    );
}
