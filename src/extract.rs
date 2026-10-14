use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rnix::ast::{Attr, AttrSet, AttrpathValue, Expr, HasEntry};
use rnix::SyntaxKind;
use rowan::ast::AstNode;

use crate::model::Record;

/// Statically collect every config assignment written in the flake's own
/// module files, then resolve same-flake imports. The flake (via
/// flake.modules.*) expresses each NixOS or home-manager module body as the
/// value of a `flake.modules.<kind>.<name>` binding; we parse each file and
/// walk those module bodies, recording each literal attribute path with its
/// source span. Records are prefixed with their flake-output path, so the
/// tree root lists the flake outputs (modules, nixosConfigurations, ...).
///
/// Any `self.<output>.<path>` reference found in an `imports` or `modules`
/// list makes the referrer pull the referenced output's records in
/// transitively; those merged records keep their source in `origin`.
pub fn scan_repo(root: &str) -> Result<(Vec<Record>, Vec<String>), String> {
    let mut files = Vec::new();
    collect_nix_files(Path::new(root), &mut files)?;

    let mut raw = Vec::new();
    let mut edges: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut targets = Vec::new();
    for file in &files {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
        };
        scan_file(file, &text, &mut raw, &mut edges, &mut targets);
    }
    targets.sort();
    targets.dedup();
    Ok((resolve(raw, edges), targets))
}

fn collect_nix_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name == "flake.nix" || name.starts_with('_') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if name.starts_with('.') || matches!(name.as_str(), "result" | "target") {
                continue;
            }
            collect_nix_files(&path, out)?;
        } else if path.extension().map(|e| e == "nix").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

fn scan_file(
    file: &Path,
    text: &str,
    out: &mut Vec<Record>,
    edges: &mut Vec<(Vec<String>, Vec<String>)>,
    targets: &mut Vec<String>,
) {
    let root = match rnix::Root::parse(text).ok() {
        Ok(root) => root,
        Err(_) => return,
    };

    for node in root.syntax().descendants() {
        if node.kind() != SyntaxKind::NODE_ATTRPATH_VALUE {
            continue;
        }
        let Some(binding) = AttrpathValue::cast(node) else {
            continue;
        };
        let Some(path) = binding.attrpath() else {
            continue;
        };
        let comps: Vec<String> = path.attrs().filter_map(|a| attr_component(&a)).collect();
        if comps.first().map(String::as_str) != Some("flake") || comps.len() < 3 {
            continue;
        }
        let file_str = file.display().to_string();

        if comps[1] == "modules" {
            // module-config output: descend into the module body's attrset
            let mut entity = vec!["modules".to_string()];
            entity.extend_from_slice(&comps[2..]);
            if let Some(body) = unwrap_body(binding.value()) {
                if let Expr::AttrSet(set) = &body {
                    collect_attrset(text, &file_str, set, &entity, out);
                }
                collect_edges(&body, &entity, edges);
            }
            continue;
        }

        // any other flake output: record the whole binding as a key value
        let entity: Vec<String> = comps[1..].to_vec();
        if comps[1] == "nixosConfigurations" {
            targets.push(format!("nixos:{}", comps[2]));
        } else if comps[1] == "homeConfigurations" {
            targets.push(format!("home:{}", comps[2]));
        } else if comps[1] == "darwinConfigurations" {
            targets.push(format!("darwin:{}", comps[2]));
        }

        let Some(value) = binding.value() else {
            continue;
        };
        collect_edges(&value, &entity, edges);
        push_record(text, &file_str, &entity, &value, out);
    }
}

/// Peel lambda / with / let / paren wrappers until we reach the true module
/// body expression.
fn unwrap_body(expr: Option<Expr>) -> Option<Expr> {
    let mut e = expr?;
    loop {
        e = match e {
            Expr::Lambda(l) => l.body()?,
            Expr::LetIn(l) => l.body()?,
            Expr::With(w) => w.body()?,
            Expr::Paren(p) => p.expr()?,
            other => return Some(other),
        };
    }
}

fn collect_attrset(
    text: &str,
    file: &str,
    set: &AttrSet,
    prefix: &[String],
    out: &mut Vec<Record>,
) {
    for binding in set.attrpath_values() {
        let Some(path) = binding.attrpath() else {
            continue;
        };
        let mut comps = Vec::new();
        let mut dynamic = false;
        for attr in path.attrs() {
            match attr_component(&attr) {
                Some(c) => comps.push(c),
                None => {
                    dynamic = true;
                    break;
                }
            }
        }
        if dynamic || comps.is_empty() || comps[0] == "imports" {
            continue;
        }
        let mut full = prefix.to_vec();
        full.extend(comps);

        let Some(value) = binding.value() else {
            continue;
        };
        match peel_value(value) {
            Some(Expr::AttrSet(nested)) => collect_attrset(text, file, &nested, &full, out),
            Some(inner) => push_record(text, file, &full, &inner, out),
            None => {}
        }
    }
}

fn push_record(text: &str, file: &str, segments: &[String], value: &Expr, out: &mut Vec<Record>) {
    let value_node = value.syntax();
    let range = value_node.text_range();
    let byte_start = usize::from(range.start());
    let byte_end = usize::from(range.end());
    let (line, col) = line_col(text, byte_start);
    let mut refs = Vec::new();
    collect_value_refs(value, &mut refs);
    refs.sort();
    refs.dedup();
    out.push(Record {
        segments: segments.to_vec(),
        file: file.to_string(),
        line,
        col,
        byte_start,
        byte_end,
        value: value_node.text().to_string(),
        origin: String::new(),
        refs,
    });
}

/// A component of a select chain rooted at the module's `self` argument.
enum AttrPart {
    Static(String),
    Dynamic,
}

/// Parse a literal select chain rooted at `self`, returning each component.
fn select_path(expr: &Expr) -> Option<Vec<AttrPart>> {
    match expr {
        Expr::Ident(ident) => {
            if ident.syntax().text().to_string() == "self" {
                Some(Vec::new())
            } else {
                None
            }
        }
        Expr::Select(sel) => {
            let mut base = select_path(&sel.expr()?)?;
            let path = sel.attrpath()?;
            for attr in path.attrs() {
                base.push(match attr {
                    Attr::Ident(i) => AttrPart::Static(i.syntax().text().to_string()),
                    Attr::Str(s) => {
                        let raw = s.syntax().text().to_string();
                        AttrPart::Static(raw.trim_matches('"').to_string())
                    }
                    Attr::Dynamic(_) => AttrPart::Dynamic,
                });
            }
            Some(base)
        }
        Expr::Paren(p) => select_path(&p.expr()?),
        _ => None,
    }
}

/// The fully-static prefix of a `self.<...>` chain, if every component is a
/// literal attribute. Dynamic components (e.g. `${system}`) yield `None`.
fn static_self_path(expr: &Expr) -> Option<Vec<String>> {
    let parts = select_path(expr)?;
    if parts.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in parts {
        match part {
            AttrPart::Static(s) => out.push(s),
            AttrPart::Dynamic => return None,
        }
    }
    Some(out)
}

/// Record every outer `self.<...>` reference occurring in a value literal.
fn collect_value_refs(expr: &Expr, refs: &mut Vec<String>) {
    if select_path(expr).is_some() {
        let text = expr.syntax().text().to_string();
        if text != "self" {
            refs.push(text);
        }
        return;
    }
    for child in expr.syntax().children() {
        if let Some(e) = Expr::cast(child) {
            collect_value_refs(&e, refs);
        }
    }
}

/// Find `imports` / `modules` / `home-manager.sharedModules` lists anywhere in
/// an output's value and record each imported output as an edge.
fn collect_edges(expr: &Expr, entity: &[String], edges: &mut Vec<(Vec<String>, Vec<String>)>) {
    for node in expr.syntax().descendants() {
        if node.kind() != SyntaxKind::NODE_ATTRPATH_VALUE {
            continue;
        }
        let Some(binding) = AttrpathValue::cast(node) else {
            continue;
        };
        let Some(path) = binding.attrpath() else {
            continue;
        };
        let names: Vec<String> = path.attrs().filter_map(|a| attr_component(&a)).collect();
        let first = names.first().map(String::as_str);
        let last = names.last().map(String::as_str);
        if first != Some("imports") && first != Some("modules") && last != Some("sharedModules") {
            continue;
        }
        let Some(value) = binding.value() else {
            continue;
        };
        edges_from_list(&value, entity, &[], edges);
    }
}

/// Walk an `imports`/`modules` value, tracking `with self.<output>;` scopes so
/// bare identifiers in the list resolve against that output (e.g.
/// `imports = with self.modules.nixos; [ common audio ]`).
fn edges_from_list(
    expr: &Expr,
    entity: &[String],
    scopes: &[Vec<String>],
    edges: &mut Vec<(Vec<String>, Vec<String>)>,
) {
    match expr {
        Expr::With(w) => {
            let mut scopes = scopes.to_vec();
            if let Some(ns) = w.namespace().and_then(|n| static_self_path(&n)) {
                scopes.push(ns);
            }
            if let Some(body) = w.body() {
                edges_from_list(&body, entity, &scopes, edges);
            }
        }
        Expr::LetIn(l) => {
            if let Some(body) = l.body() {
                edges_from_list(&body, entity, scopes, edges);
            }
        }
        Expr::Paren(p) => {
            if let Some(e) = p.expr() {
                edges_from_list(&e, entity, scopes, edges);
            }
        }
        Expr::BinOp(op) => {
            if let Some(l) = op.lhs() {
                edges_from_list(&l, entity, scopes, edges);
            }
            if let Some(r) = op.rhs() {
                edges_from_list(&r, entity, scopes, edges);
            }
        }
        Expr::List(list) => {
            for item in list.items() {
                if let Some(target) = static_self_path(&item) {
                    edges.push((entity.to_vec(), target));
                } else if let Expr::Ident(id) = &item {
                    let name = id.syntax().text().to_string();
                    for scope in scopes {
                        let mut target = scope.clone();
                        target.push(name.clone());
                        edges.push((entity.to_vec(), target));
                    }
                }
            }
        }
        _ => {}
    }
}

/// Flat-merge every transitive import under each importing output. Original
/// records stay in place; merged copies are prefixed with the importer and
/// carry their source output in `origin`.
fn resolve(raw: Vec<Record>, edges: Vec<(Vec<String>, Vec<String>)>) -> Vec<Record> {
    let mut by_entity: HashMap<Vec<String>, Vec<Record>> = HashMap::new();
    for rec in &raw {
        by_entity
            .entry(entity_key(&rec.segments))
            .or_default()
            .push(rec.clone());
    }

    let mut adj: HashMap<Vec<String>, Vec<Vec<String>>> = HashMap::new();
    for (from, to) in edges {
        adj.entry(from).or_default().push(to);
    }

    let mut out = raw;
    let mut roots: Vec<Vec<String>> = adj.keys().cloned().collect();
    roots.sort();
    for root in roots {
        for target in reachable(&root, &adj) {
            let Some(records) = by_entity.get(&target) else {
                continue;
            };
            let depth = target.len();
            for rec in records {
                if rec.segments.len() < depth {
                    continue;
                }
                let mut merged = rec.clone();
                let mut segments = root.clone();
                segments.extend_from_slice(&rec.segments[depth..]);
                merged.segments = segments;
                merged.origin = target.join(".");
                out.push(merged);
            }
        }
    }
    out
}

/// Entity key: modules are identified by `modules.<kind>.<name>`, every other
/// output by its whole binding path.
fn entity_key(segments: &[String]) -> Vec<String> {
    if segments.first().map(String::as_str) == Some("modules") && segments.len() >= 3 {
        segments[..3].to_vec()
    } else {
        segments.to_vec()
    }
}

/// Breadth of a root's transitive imports, each entity visited once.
fn reachable(start: &[String], adj: &HashMap<Vec<String>, Vec<Vec<String>>>) -> Vec<Vec<String>> {
    let mut seen: HashSet<Vec<String>> = HashSet::new();
    let mut stack: Vec<Vec<String>> = vec![start.to_vec()];
    let mut order = Vec::new();
    while let Some(node) = stack.pop() {
        let Some(next) = adj.get(&node) else {
            continue;
        };
        for n in next {
            if n.as_slice() == start || !seen.insert(n.clone()) {
                continue;
            }
            order.push(n.clone());
            stack.push(n.clone());
        }
    }
    order
}

/// Peel `with ...;` / `let ... in` / paren wrappers so the recorded value
/// (and edit span) points at the inner expression instead of the wrapper.
fn peel_value(expr: Expr) -> Option<Expr> {
    let mut e = expr;
    loop {
        e = match e {
            Expr::With(w) => w.body()?,
            Expr::LetIn(l) => l.body()?,
            Expr::Paren(p) => p.expr()?,
            other => return Some(other),
        };
    }
}

fn attr_component(attr: &Attr) -> Option<String> {
    match attr {
        Attr::Ident(ident) => Some(ident.syntax().text().to_string()),
        Attr::Str(s) => {
            let raw = s.syntax().text().to_string();
            Some(raw.trim_matches('"').to_string())
        }
        Attr::Dynamic(_) => None,
    }
}

fn line_col(text: &str, offset: usize) -> (u32, u32) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_wrapped_module() {
        let text = r#"{ self, inputs, ... }: {
  flake.modules.nixos.audio = { config, pkgs, ... }: {
    services.pipewire = {
      enable = true;
      alsa.enable = true;
    };
    environment.systemPackages = with pkgs; [ pipewire ];
  };
}"#;
        let mut out = Vec::new();
        let mut edges = Vec::new();
        let mut targets = Vec::new();
        scan_file(Path::new("test.nix"), text, &mut out, &mut edges, &mut targets);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|r| r.segments.join(".")).collect::<Vec<_>>(),
            vec![
                "modules.nixos.audio.services.pipewire.enable",
                "modules.nixos.audio.services.pipewire.alsa.enable",
                "modules.nixos.audio.environment.systemPackages"
            ]
        );
        assert_eq!(out[0].value, "true");
        assert_eq!(out[2].value, "[ pipewire ]");
    }

    #[test]
    fn resolves_host_imports() {
        let text = r#"{ self, ... }: {
  flake.nixosConfigurations.host = lib.nixosSystem {
    modules = [ self.modules.nixos.base ];
  };
  flake.modules.nixos.base = { ... }: {
    imports = [ self.modules.nixos.extra ];
    services.base.enable = true;
  };
  flake.modules.nixos.extra = { ... }: {
    services.extra.enable = true;
  };
}"#;
        let mut raw = Vec::new();
        let mut edges = Vec::new();
        let mut targets = Vec::new();
        scan_file(Path::new("test.nix"), text, &mut raw, &mut edges, &mut targets);
        assert_eq!(targets, vec!["nixos:host".to_string()]);
        let out = resolve(raw, edges);
        let merged: Vec<(String, String)> = out
            .iter()
            .filter(|r| !r.origin.is_empty())
            .map(|r| (r.segments.join("."), r.origin.clone()))
            .collect();
        assert!(merged.contains(&(
            "nixosConfigurations.host.services.base.enable".to_string(),
            "modules.nixos.base".to_string()
        )));
        assert!(merged.contains(&(
            "nixosConfigurations.host.services.extra.enable".to_string(),
            "modules.nixos.extra".to_string()
        )));
        assert!(merged.contains(&(
            "modules.nixos.base.services.extra.enable".to_string(),
            "modules.nixos.extra".to_string()
        )));
    }

    #[test]
    fn resolves_with_scoped_imports() {
        let text = r#"{ self, ... }: {
  flake.nixosConfigurations.host = lib.nixosSystem {
    modules = [ self.modules.nixos.base ];
  };
  flake.modules.nixos.base = { ... }: {
    imports = with self.modules.nixos; [ common extra ];
    services.base.enable = true;
  };
  flake.modules.nixos.common = { ... }: {
    environment.systemPackages = [ pkgs.wget ];
  };
  flake.modules.nixos.extra = { ... }: {
    services.extra.enable = true;
  };
}"#;
        let mut raw = Vec::new();
        let mut edges = Vec::new();
        let mut targets = Vec::new();
        scan_file(Path::new("test.nix"), text, &mut raw, &mut edges, &mut targets);
        let out = resolve(raw, edges);
        let merged: Vec<(String, String)> = out
            .iter()
            .filter(|r| !r.origin.is_empty())
            .map(|r| (r.segments.join("."), r.origin.clone()))
            .collect();
        assert!(merged.contains(&(
            "nixosConfigurations.host.environment.systemPackages".to_string(),
            "modules.nixos.common".to_string()
        )));
        assert!(merged.contains(&(
            "nixosConfigurations.host.services.extra.enable".to_string(),
            "modules.nixos.extra".to_string()
        )));
    }

    #[test]
    fn records_value_refs() {
        let text = r#"{ self, ... }: {
  flake.modules.nixos.zen = { pkgs, ... }: {
    environment.systemPackages = [ self.packages.${pkgs.system}.zen-browser ];
  };
}"#;
        let mut out = Vec::new();
        let mut edges = Vec::new();
        let mut targets = Vec::new();
        scan_file(Path::new("test.nix"), text, &mut out, &mut edges, &mut targets);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].refs,
            vec!["self.packages.${pkgs.system}.zen-browser".to_string()]
        );
    }
}
