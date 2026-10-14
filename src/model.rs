use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Record {
    pub segments: Vec<String>,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub value: String,
    /// Dotted flake-output path this record was pulled in from, e.g.
    /// "modules.nixos.common". Empty = defined directly in this output.
    pub origin: String,
    /// Same-flake output references found in this record's value literal,
    /// e.g. "self.packages.x86_64-linux.zen-browser".
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub segments: Vec<String>,
    pub ty: String,
    pub value: Option<String>,
    pub records: Vec<Record>,
}

impl Node {
    pub fn is_folder(&self) -> bool {
        self.ty == "namespace"
    }

    pub fn short_type(&self) -> &str {
        if self.ty == "namespace" {
            "namespace"
        } else {
            "key"
        }
    }
}

/// Group records into parent-path -> children nodes. Namespaces are the
/// dotted-path prefixes of every defined key; keys are the leaf assignments.
pub fn build_levels(records: &[Record]) -> HashMap<String, Vec<Node>> {
    let mut levels: HashMap<String, Vec<Node>> = HashMap::new();

    fn merge(nodes: &mut Vec<Node>, node: Node) {
        match nodes.iter_mut().find(|n| n.path == node.path) {
            Some(existing) => {
                if node.ty == "namespace" && existing.ty == "key" {
                    existing.ty = "namespace".to_string();
                } else if node.ty == "key" && existing.ty == "key" {
                    existing.records.extend(node.records);
                    if existing.value.is_none() {
                        existing.value = node.value;
                    }
                }
            }
            None => nodes.push(node),
        }
    }

    for rec in records {
        let segs = &rec.segments;
        for depth in 1..=segs.len() {
            let parent = &segs[..depth - 1];
            let parent_key = parent.join(".");
            let path = segs[..depth].join(".");
            let name = segs[depth - 1].clone();
            let segments = segs[..depth].to_vec();

            if depth < segs.len() {
                let node = Node {
                    ty: "namespace".to_string(),
                    name,
                    path,
                    segments,
                    value: None,
                    records: Vec::new(),
                };
                merge(levels.entry(parent_key).or_default(), node);
            } else {
                let node = Node {
                    ty: "key".to_string(),
                    value: Some(rec.value.clone()),
                    records: vec![rec.clone()],
                    name,
                    path,
                    segments,
                };
                merge(levels.entry(parent_key).or_default(), node);
            }
        }
    }

    levels
}