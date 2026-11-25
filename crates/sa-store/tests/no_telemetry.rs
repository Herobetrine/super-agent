use proc_macro2::{Delimiter, TokenStream, TokenTree};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use syn::parse::{Parse, ParseStream, Parser};
use toml_edit::{Document, Item, TableLike};
use url::Url;

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

// Match one Cargo glob component at a time, so a single "*" never crosses a directory.
fn member_component(pattern: &str) -> Regex {
    let mut expression = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            '[' => {
                let negated = chars.next_if_eq(&'!').is_some();
                let mut class = String::new();
                for ch in chars.by_ref() {
                    if ch == ']' && !class.is_empty() {
                        break;
                    }
                    if ch == '-' {
                        class.push(ch);
                    } else {
                        class.push_str(&regex::escape(&ch.to_string()));
                    }
                }
                expression.push('[');
                if negated {
                    expression.push('^');
                }
                expression.push_str(&class);
                expression.push(']');
            }
            ch => expression.push_str(&regex::escape(&ch.to_string())),
        }
    }
    expression.push('$');
    Regex::new(&expression)
        .unwrap_or_else(|err| panic!("invalid workspace glob {pattern:?}: {err}"))
}

fn expand_member_paths(dir: &Path, parts: &[&str], paths: &mut HashSet<PathBuf>) {
    let Some((part, rest)) = parts.split_first() else {
        if dir.is_dir() {
            paths.insert(dir.canonicalize().unwrap());
        }
        return;
    };
    if !part.contains(['*', '?', '[']) {
        expand_member_paths(&dir.join(part), rest, paths);
        return;
    }
    let recursive = *part == "**";
    if recursive {
        expand_member_paths(dir, rest, paths);
    }
    if !dir.is_dir() {
        return;
    }
    let pattern = member_component(part);
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        // Recursive globs follow real directories only, avoiding symlink cycles.
        let is_dir = if recursive {
            entry.file_type().unwrap().is_dir()
        } else {
            path.is_dir()
        };
        if is_dir && pattern.is_match(&entry.file_name().to_string_lossy()) {
            expand_member_paths(&path, if recursive { parts } else { rest }, paths);
        }
    }
}

fn workspace_member_manifests(root: &Path, workspace: &dyn TableLike) -> Vec<PathBuf> {
    let expand = |key| {
        let mut paths = HashSet::new();
        if let Some(entries) = workspace.get(key).and_then(Item::as_array) {
            for entry in entries.iter() {
                let pattern = entry.as_str().expect("workspace path must be a string");
                let parts: Vec<_> = pattern.split('/').filter(|part| !part.is_empty()).collect();
                expand_member_paths(root, &parts, &mut paths);
            }
        }
        paths
    };
    let excluded = expand("exclude");
    let mut manifests: Vec<_> = expand("members")
        .into_iter()
        .filter(|path| !excluded.contains(path) && path != root)
        .map(|path| path.join("Cargo.toml"))
        .filter(|path| path.is_file())
        .collect();
    manifests.sort_unstable();
    manifests
}

fn check_manifest_file(doc: &Document<&str>, text: &str, path: &Path, findings: &mut Vec<String>) {
    check_manifest(doc.as_table(), text, path, findings);
    let name = doc
        .get("package")
        .and_then(Item::as_table_like)
        .and_then(|package| package.get("name"));
    if let Some((item, name)) = name.and_then(|item| item.as_str().map(|name| (item, name))) {
        check_crate(name, item.span(), text, path, findings);
    }
}

fn check_manifests(root: &Path, findings: &mut Vec<String>) -> Vec<PathBuf> {
    let lock = root.join("Cargo.lock");
    let text = read(&lock);
    let doc = Document::parse(text.as_str()).expect("parse Cargo.lock");
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

    let text = read(&root.join("Cargo.toml"));
    let doc = Document::parse(text.as_str()).expect("parse root Cargo.toml");
    check_manifest_file(&doc, &text, Path::new("Cargo.toml"), findings);
    let workspace = doc
        .get("workspace")
        .and_then(Item::as_table_like)
        .expect("root workspace table");
    let members = workspace_member_manifests(root, workspace);
    for path in &members {
        let display_path = path.strip_prefix(root).unwrap_or(path);
        let text = read(path);
        let doc = Document::parse(text.as_str())
            .unwrap_or_else(|err| panic!("{}: {err}", display_path.display()));
        check_manifest_file(&doc, &text, display_path, findings);
    }
    let mut source_members: Vec<_> = members
        .iter()
        .map(|path| path.parent().unwrap().to_path_buf())
        .collect();
    if doc.get("package").is_some() {
        source_members.push(root.to_path_buf());
    }
    source_members
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

enum AllowedUrl<'a> {
    Exact(&'a str),
    Prefix(Url),
}

impl<'a> AllowedUrl<'a> {
    fn parse(entry: &'a str) -> Option<Self> {
        let prefix = entry.strip_prefix("prefix ");
        let literal = prefix.unwrap_or(entry).trim();
        if !(literal.starts_with("http://") || literal.starts_with("https://"))
            || literal.contains('*')
        {
            return None;
        }
        let Some(_) = prefix else {
            return Some(Self::Exact(literal));
        };
        // A template can only be approved as a whole, exact literal.
        if literal.contains(['{', '}']) {
            return None;
        }
        let url = Url::parse(literal).ok()?;
        (url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none())
        .then_some(Self::Prefix(url))
    }

    fn allows(&self, candidate: &str) -> bool {
        match self {
            Self::Exact(entry) => candidate == *entry,
            Self::Prefix(approved) => {
                !candidate.contains(['{', '}'])
                    && Url::parse(candidate).is_ok_and(|candidate| {
                        candidate.scheme() == approved.scheme()
                            && candidate.host() == approved.host()
                            && candidate.port_or_known_default() == approved.port_or_known_default()
                            && candidate.username().is_empty()
                            && candidate.password().is_none()
                            && candidate.path().starts_with(approved.path())
                    })
            }
        }
    }
}

fn allowlist() -> Vec<AllowedUrl<'static>> {
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
            Some(AllowedUrl::parse(entry).filter(|_| !reason.trim().is_empty()).unwrap_or_else(|| {
                panic!("allowed-urls.txt:{}: use an exact URL or a concrete URL prefix and a reason", index + 1)
            }))
        })
        .collect()
}

fn ident(token: &TokenTree, expected: &str) -> bool {
    matches!(token, TokenTree::Ident(name) if name == expected)
}

// Evaluate with test=false and every platform/feature flag unknown.
fn cfg_without_test(predicate: &[TokenTree]) -> Option<bool> {
    match predicate {
        [name] if ident(name, "test") => Some(false),
        [name, TokenTree::Group(group)] if group.delimiter() == Delimiter::Parenthesis => {
            let args: Vec<_> = group.stream().into_iter().collect();
            let comma = |token: &TokenTree| matches!(token, TokenTree::Punct(mark) if mark.as_char() == ',');
            let args = match args.as_slice() {
                [args @ .., last] if comma(last) => args,
                args => args,
            };
            if ident(name, "not") {
                return cfg_without_test(args).map(|value| !value);
            }
            if !ident(name, "all") && !ident(name, "any") {
                return None;
            }
            let identity = ident(name, "all");
            if args.is_empty() {
                return Some(identity);
            }
            let mut value = Some(identity);
            for arg in args.split(comma) {
                match cfg_without_test(arg) {
                    Some(arg) if arg != identity => return Some(arg),
                    None => value = None,
                    _ => {}
                }
            }
            value
        }
        _ => None,
    }
}

fn cfg_test(attribute: &proc_macro2::Group) -> bool {
    let tokens: Vec<_> = attribute.stream().into_iter().collect();
    match tokens.as_slice() {
        [name, TokenTree::Group(args)]
            if ident(name, "cfg") && args.delimiter() == Delimiter::Parenthesis =>
        {
            let args: Vec<_> = args.stream().into_iter().collect();
            let predicate = match args.as_slice() {
                [args @ .., TokenTree::Punct(comma)] if comma.as_char() == ',' => args,
                args => args,
            };
            cfg_without_test(predicate) == Some(false)
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

struct Module<'a> {
    attributes: &'a [TokenTree],
    name: &'a proc_macro2::Ident,
    end: &'a TokenTree,
    after: &'a [TokenTree],
    test_only: bool,
    explicit_path: Option<PathBuf>,
}

struct ModuleLink {
    inline_dir: Option<PathBuf>,
    name: String,
    explicit_path: Option<PathBuf>,
    test_only: bool,
}

#[derive(Default)]
struct SourceScan {
    urls: Vec<(usize, String)>,
    modules: Vec<ModuleLink>,
}

fn is_test_attribute(attribute: &proc_macro2::Group) -> bool {
    cfg_test(attribute)
        || matches!(
            attribute.stream().into_iter().collect::<Vec<_>>().as_slice(),
            [name] if ident(name, "test")
        )
}

// Keep attributes, visibility and the module item together so a cfg applies
// only to its own module. Both test and potentially production references count.
fn module_declaration(tokens: &[TokenTree]) -> Option<Module<'_>> {
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
    let attributes = &tokens[..tokens.len() - rest.len()];
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
    let [keyword, TokenTree::Ident(name), end, after @ ..] = rest else {
        return None;
    };
    if !ident(keyword, "mod") {
        return None;
    }
    Some(Module {
        attributes,
        name,
        end,
        after,
        test_only,
        explicit_path,
    })
}

fn test_item_len(tokens: &[TokenTree]) -> Option<usize> {
    let mut rest = tokens;
    let mut test_only = false;
    while let [
        TokenTree::Punct(hash),
        TokenTree::Group(attribute),
        tail @ ..,
    ] = rest
    {
        if hash.as_char() != '#' || attribute.delimiter() != Delimiter::Bracket {
            break;
        }
        test_only |= is_test_attribute(attribute);
        rest = tail;
    }
    if !test_only {
        return None;
    }
    fn consumed<T: Parse>(tokens: &[TokenTree]) -> Option<usize> {
        let parse_one = |input: ParseStream<'_>| {
            input.parse::<T>()?;
            input.parse::<TokenStream>()
        };
        let remaining = parse_one.parse2(tokens.iter().cloned().collect()).ok()?;
        let consumed = tokens.len() - remaining.into_iter().count();
        (consumed > 0).then_some(consumed)
    }
    let item = consumed::<syn::Item>(rest)
        .or_else(|| consumed::<syn::ImplItem>(rest))
        .or_else(|| consumed::<syn::TraitItem>(rest))
        .or_else(|| consumed::<syn::ForeignItem>(rest))?;
    Some(tokens.len() - rest.len() + item)
}

fn concat_call(tokens: &[TokenTree]) -> Option<(&proc_macro2::Group, &[TokenTree])> {
    match tokens {
        [
            name,
            TokenTree::Punct(bang),
            TokenTree::Group(args),
            tail @ ..,
        ] if ident(name, "concat") && bang.as_char() == '!' => Some((args, tail)),
        _ => None,
    }
}

fn concat_literal(args: &proc_macro2::Group) -> Option<String> {
    let tokens: Vec<_> = args.stream().into_iter().collect();
    let comma =
        |token: &TokenTree| matches!(token, TokenTree::Punct(mark) if mark.as_char() == ',');
    let args = match tokens.as_slice() {
        [args @ .., last] if comma(last) => args,
        args => args,
    };
    let mut result = String::new();
    if args.is_empty() {
        return Some(result);
    }
    for arg in args.split(comma) {
        if let Some((nested, [])) = concat_call(arg) {
            result.push_str(&concat_literal(nested)?);
        } else {
            let literal = syn::parse2(arg.iter().cloned().collect()).ok()?;
            let value = match literal {
                syn::Lit::Str(value) => value.value(),
                syn::Lit::Char(value) => value.value().to_string(),
                syn::Lit::Bool(value) => value.value.to_string(),
                syn::Lit::Int(value) => value.base10_digits().to_owned(),
                syn::Lit::Float(value) => {
                    value.to_string().strip_suffix(value.suffix())?.to_owned()
                }
                _ => return None,
            };
            result.push_str(&value);
        }
    }
    Some(result)
}

fn scan_tokens(
    tokens: &[TokenTree],
    inline_dir: Option<&Path>,
    test_only: bool,
    modules: &mut Vec<ModuleLink>,
    visit: &mut impl FnMut(usize, &str),
) {
    let mut rest = tokens;
    while let [first, tail @ ..] = rest {
        if let Some(module) = module_declaration(rest) {
            let name = module.name.to_string();
            let name = name.strip_prefix("r#").unwrap_or(&name);
            let test_only = test_only || module.test_only;
            if !test_only {
                scan_tokens(module.attributes, inline_dir, false, modules, visit);
            }
            match module.end {
                TokenTree::Group(body) if body.delimiter() == Delimiter::Brace => {
                    let nested = inline_dir.unwrap_or_else(|| Path::new("")).join(
                        module
                            .explicit_path
                            .as_deref()
                            .unwrap_or_else(|| Path::new(name)),
                    );
                    let body: Vec<_> = body.stream().into_iter().collect();
                    scan_tokens(&body, Some(&nested), test_only, modules, visit);
                    rest = module.after;
                    continue;
                }
                TokenTree::Punct(end) if end.as_char() == ';' => {
                    modules.push(ModuleLink {
                        inline_dir: inline_dir.map(Path::to_path_buf),
                        name: name.to_owned(),
                        explicit_path: module.explicit_path,
                        test_only,
                    });
                    rest = module.after;
                    continue;
                }
                _ => {}
            }
        }
        if matches!(first, TokenTree::Punct(hash) if hash.as_char() == '#')
            && let Some(len) = test_item_len(rest)
        {
            rest = &rest[len..];
            continue;
        }
        if !test_only
            && let Some((args, after)) = concat_call(rest)
            && let Some(value) = concat_literal(args)
        {
            visit(first.span().start().line, &value);
            rest = after;
            continue;
        }
        match first {
            TokenTree::Group(group) => {
                let nested: Vec<_> = group.stream().into_iter().collect();
                scan_tokens(&nested, inline_dir, test_only, modules, visit);
            }
            TokenTree::Literal(literal) if !test_only => {
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
    let root = crate_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let mut findings = Vec::new();
    let members = check_manifests(&root, &mut findings);
    check_sources(&root, &members, &allowlist(), &mut findings);
    findings.sort_unstable();
    findings.dedup();
    assert!(
        findings.is_empty(),
        "{}\nRemove forbidden crates/hosts. Remove an unapproved URL or add it to crates/sa-store/tests/allowed-urls.txt with a reason in review.",
        findings.join("\n")
    );
}

fn crate_entry_point(relative: &Path) -> bool {
    let parts: Vec<_> = relative.iter().collect();
    match parts.as_slice() {
        [name] => *name == "lib.rs" || *name == "main.rs",
        [bin, _] => *bin == "bin",
        [bin, _, name] => *bin == "bin" && *name == "main.rs",
        _ => false,
    }
}

// A file may be entered from several crates or under more than one module
// path. Traverse each context while retaining any production reachability.
fn module_reachability(
    roots: Vec<PathBuf>,
    sources: &HashMap<PathBuf, SourceScan>,
) -> HashMap<PathBuf, bool> {
    let mut pending: Vec<_> = roots
        .into_iter()
        .map(|path| {
            let dir = path.parent().unwrap().to_path_buf();
            (path, dir, false)
        })
        .collect();
    let mut visited = HashSet::new();
    let mut test_only_files = HashMap::new();
    while let Some((file, module_dir, test_only)) = pending.pop() {
        if !visited.insert((file.clone(), module_dir.clone(), test_only)) {
            continue;
        }
        let Some(source) = sources.get(&file) else {
            continue;
        };
        test_only_files
            .entry(file.clone())
            .and_modify(|flag| *flag &= test_only)
            .or_insert(test_only);
        for link in &source.modules {
            let inherited = test_only || link.test_only;
            let base = link
                .inline_dir
                .as_ref()
                .map_or_else(|| module_dir.clone(), |inline| module_dir.join(inline));
            if let Some(path) = &link.explicit_path {
                let path_base = if link.inline_dir.is_some() {
                    &base
                } else {
                    file.parent().unwrap()
                };
                if let Ok(path) = path_base.join(path).canonicalize() {
                    let child_dir = path.parent().unwrap().to_path_buf();
                    pending.push((path, child_dir, inherited));
                }
            } else {
                let child_dir = base.join(&link.name);
                for path in [
                    base.join(format!("{}.rs", link.name)),
                    child_dir.join("mod.rs"),
                ] {
                    if let Ok(path) = path.canonicalize() {
                        pending.push((path, child_dir.clone(), inherited));
                    }
                }
            }
        }
    }
    test_only_files
}

fn check_sources(
    root: &Path,
    members: &[PathBuf],
    allowlist: &[AllowedUrl<'_>],
    findings: &mut Vec<String>,
) {
    let hosts: Vec<_> = FORBIDDEN_HOST_LABELS
        .iter()
        .map(|(host, tld)| format!("{host}.{tld}"))
        .chain(std::iter::once(["datadog", "hq"].concat()))
        .collect();
    let url = Regex::new(r#"(?i)https?://[^\s"'<>]*"#).unwrap();
    let mut roots = Vec::new();
    let mut sources = HashMap::new();
    for crate_dir in members {
        for area in ["src", "tests"] {
            let dir = crate_dir.join(area);
            if !dir.is_dir() {
                continue;
            }
            visit_files(&dir, &mut |path| {
                let relative = path.strip_prefix(root).unwrap_or(path);
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
                if area == "src" && path.extension().is_some_and(|extension| extension == "rs") {
                    let source_path = path.canonicalize().unwrap();
                    if crate_entry_point(path.strip_prefix(&dir).unwrap()) {
                        roots.push(source_path.clone());
                    }
                    let tokens = text
                        .parse::<TokenStream>()
                        .unwrap_or_else(|err| panic!("{}: {err}", relative.display()));
                    let tokens: Vec<_> = tokens.into_iter().collect();
                    let mut source = SourceScan::default();
                    scan_tokens(
                        &tokens,
                        None,
                        false,
                        &mut source.modules,
                        &mut |line, literal| {
                            for candidate in url.find_iter(literal).map(|url| url.as_str()) {
                                if !allowlist.iter().any(|rule| rule.allows(candidate)) {
                                    source.urls.push((line, candidate.to_owned()));
                                }
                            }
                        },
                    );
                    sources.insert(source_path, source);
                }
            });
        }
    }
    let reachability = module_reachability(roots, &sources);
    for (path, source) in sources {
        // Keep scanning orphan files unless a test-only path is proven.
        if reachability.get(&path) == Some(&true) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        for (line, candidate) in source.urls {
            findings.push(format!(
                "{}:{line}: URL needs review: {candidate}",
                relative.display()
            ));
        }
    }
}

#[test]
fn workspace_member_resolution_uses_declared_globs_paths_and_excludes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let manifest = r#"
        [workspace]
        members = ["crates/*", "tools/explicit", "plugins/**/member-?"]
        exclude = ["crates/excluded"]
    "#;
    let doc = Document::parse(manifest).unwrap();
    for subdir in [
        "crates/included",
        "crates/excluded",
        "tools/explicit",
        "plugins/nested/member-a",
        "plugins/nested/member-extra",
        "workspaces/another-project",
    ] {
        let path = root.join(subdir);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("Cargo.toml"), "[package]\nname = 'fixture'\n").unwrap();
    }
    let workspace = doc.get("workspace").and_then(Item::as_table_like).unwrap();
    let members = workspace_member_manifests(&root, workspace);
    assert_eq!(
        members,
        [
            root.join("crates/included/Cargo.toml"),
            root.join("plugins/nested/member-a/Cargo.toml"),
            root.join("tools/explicit/Cargo.toml"),
        ],
    );
    fs::write(root.join("Cargo.toml"), manifest).unwrap();
    fs::write(root.join("Cargo.lock"), "[[package]]\nname = 'fixture'\n").unwrap();
    for member in [
        "tools/explicit",
        "crates/excluded",
        "workspaces/another-project",
    ] {
        let src = root.join(member).join("src");
        fs::create_dir(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            r#"const URL: &str = "https://example.org/unapproved";"#,
        )
        .unwrap();
    }
    let mut findings = Vec::new();
    let members = check_manifests(&root, &mut findings);
    check_sources(&root, &members, &[], &mut findings);
    assert_eq!(findings.len(), 1, "{findings:?}");
    let expected = Path::new("tools")
        .join("explicit")
        .join("src")
        .join("lib.rs");
    assert!(findings[0].starts_with(&format!("{}:1:", expected.display())));
}

#[test]
fn cfg_predicates_keep_modules_that_could_compile_without_test() {
    for (predicate, expected) in [
        ("all(test, windows)", true),
        ("any(test, windows)", false),
        ("not(test)", false),
        ("not(any(not(test), windows))", true),
        ("all(windows, feature = \"example\")", false),
    ] {
        let stream: TokenStream = format!("[cfg({predicate})]").parse().unwrap();
        let Some(TokenTree::Group(attribute)) = stream.into_iter().next() else {
            panic!("expected cfg attribute");
        };
        assert_eq!(cfg_test(&attribute), expected, "{predicate}");
    }
}

#[test]
fn url_prefix_is_confined_to_one_origin_and_a_path() {
    let prefix = AllowedUrl::parse("prefix https://api.example.com:8443/v1/").unwrap();
    assert!(prefix.allows("https://api.example.com:8443/v1/items?limit=1"));
    for rejected in [
        "https://api.example.com.invalid:8443/v1/items",
        "https://api.example.com/v1/items",
        "http://api.example.com:8443/v1/items",
        "https://api.example.com:8443/v2/items",
        "https://api.example.com:8443/v1/{item}",
    ] {
        assert!(!prefix.allows(rejected), "{rejected}");
    }
    let origin = AllowedUrl::parse("prefix https://api.example.com").unwrap();
    assert!(origin.allows("https://api.example.com:443/v1/items"));
    assert!(!origin.allows("https://api.example.com.invalid/collect"));
    let template = AllowedUrl::parse("http://{host}:{port}").unwrap();
    assert!(template.allows("http://{host}:{port}"));
    assert!(!template.allows("http://{host}:{port}/collect"));
    assert!(AllowedUrl::parse("prefix http://{host}:{port}").is_none());
}

#[test]
fn production_contests_file_is_scanned_even_when_referenced_by_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let member = root.join("crates/example");
    let src = member.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("lib.rs"),
        r#"
            mod contests;
            #[cfg(test)]
            #[path = "contests.rs"]
            mod test_contests;
            #[cfg(all(test, windows))]
            mod fixtures;
        "#,
    )
    .unwrap();
    let fixture_source = r#"const URL: &str = "https://example.com/collect";"#;
    fs::write(src.join("contests.rs"), fixture_source).unwrap();
    fs::write(src.join("fixtures.rs"), fixture_source).unwrap();
    let mut findings = Vec::new();
    check_sources(&root, &[member], &[], &mut findings);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("contests.rs:1: URL needs review"));
}

#[test]
fn concat_checks_the_assembled_url_and_keeps_the_scheme_literal_valid() {
    let source = r#"
        const SCHEME: &str = "https://";
        const BLOCKED: &str = concat!("https://", "example.org/api");
        const APPROVED: &str = concat!("https://", concat!(r"example.org", "/allowed"));
        const WITH_PORT: &str = concat!("http", 's', "://example.org:", 0x20FB, "/numbers/", -2);
    "#;
    let allowed = [
        AllowedUrl::Exact("https://"),
        AllowedUrl::Exact("https://example.org/allowed"),
        AllowedUrl::Exact("https://example.org:8443/numbers/-2"),
    ];
    let findings = check_source_fixture(&[("lib.rs", source)], &allowed);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("lib.rs:3: URL needs review: https://example.org/api"));
}

#[test]
fn test_only_paths_propagate_to_children_but_production_paths_win() {
    let test_only = r#"const URL: &str = "https://example.org/test-fixture";"#;
    let shared = r#"const URL: &str = "https://example.org/shared-fixture";"#;
    let findings = check_source_fixture(
        &[
            ("lib.rs", "#[cfg(test)]\nmod tests;"),
            ("tests.rs", "mod fixtures;\nmod shared;"),
            ("tests/fixtures.rs", test_only),
            ("tests/shared.rs", shared),
            (
                "bin/tool/main.rs",
                "#[path = \"../../tests/shared.rs\"]\nmod shared;\nfn main() {}",
            ),
        ],
        &[],
    );
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].contains("shared.rs:1: URL needs review: https://example.org/shared-fixture")
    );
}

#[test]
fn test_attributes_skip_only_the_items_they_decorate() {
    let source = r#"
        #[test]
        fn test_endpoint() { let _ = "https://example.org/test"; }
        fn production_endpoint() -> &'static str { "https://example.org/production" }
        #[cfg(test)]
        const TEST_URL: &str = { "https://example.org/cfg-test" };
        const PRODUCTION_URL: &str = "https://example.org/another-production";
        struct Sample;
        impl Sample {
            #[cfg(all(test, windows))]
            fn fixture(&self) -> &'static str { "https://example.org/method-fixture" }
        }
    "#;
    let findings = check_source_fixture(&[("lib.rs", source)], &[]);
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings[0].contains("lib.rs:4: URL needs review: https://example.org/production"));
    assert!(
        findings[1].contains("lib.rs:7: URL needs review: https://example.org/another-production")
    );
}

fn check_source_fixture(files: &[(&str, &str)], allowlist: &[AllowedUrl<'_>]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let member = root.join("tools/example");
    for (file, source) in files {
        let path = member.join("src").join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let mut findings = Vec::new();
    check_sources(&root, &[member], allowlist, &mut findings);
    findings.sort_unstable();
    findings
}
