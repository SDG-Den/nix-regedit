mod edit;
mod extract;
mod model;
mod nixd;
mod value;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use model::{Node, Record};
use value::ValueKind;

const APP_ID: &str = "org.nix_regedit.NixRegedit";

const T_NAME: u32 = 0; // tree: name
const T_ICON: u32 = 1;
const C_NAME: u32 = 0; // contents: name
const C_TYPE: u32 = 1;
const C_ICON: u32 = 2;

enum Msg {
    Scan(Result<(Vec<Record>, Vec<String>), String>),
    Edited(Result<String, String>),
    Nixd(Result<String, String>),
}

type Tx = async_channel::Sender<Msg>;

/// Sets a flag while a model is being rebuilt, so the reentrant
/// `selection-changed` signals emitted by `TreeStore::clear`/`append` are
/// ignored. Restores the previous value on drop, so nesting is safe.
struct Suppress<'a> {
    cell: &'a Cell<bool>,
    previous: bool,
}

impl<'a> Suppress<'a> {
    fn new(cell: &'a Cell<bool>) -> Self {
        let previous = cell.replace(true);
        Self { cell, previous }
    }
}

impl Drop for Suppress<'_> {
    fn drop(&mut self) {
        self.cell.set(self.previous);
    }
}

struct Ui {
    window: adw::ApplicationWindow,
    flake_combo: gtk::ComboBoxText,
    search: gtk::SearchEntry,

    // left: folder tree
    tree: gtk::TreeView,
    store: gtk::TreeStore,

    // right: contents of the selected folder
    up_btn: gtk::Button,
    crumb: gtk::Label,
    view: gtk::TreeView,
    cstore: gtk::TreeStore,

    status: gtk::Label,

    flakes: RefCell<Vec<PathBuf>>,
    flake_path: RefCell<String>,
    targets: RefCell<Vec<String>>,

    // dotted-path -> children of that folder (namespaces and keys)
    levels: RefCell<HashMap<String, Vec<Node>>>,

    // left tree: row-index key -> node; dotted path -> row-index key
    tree_map: RefCell<HashMap<String, Node>>,
    tree_rows: RefCell<HashMap<String, String>>,

    // right panel current folder (path segments) and open detail popup
    right_path: RefCell<Vec<String>>,
    view_map: RefCell<HashMap<String, Node>>,
    popup_path: RefCell<String>,
    current: RefCell<Option<Node>>,
    popup: RefCell<Option<(gtk::Dialog, Detail)>>,

    // set while rebuilding models so reentrant selection-changed signals from
    // `TreeStore::clear`/`append` are ignored
    suppress: Cell<bool>,
}

fn discover_flakes(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 3 {
            return;
        }
        if dir.join("flake.nix").is_file() {
            out.push(dir.to_path_buf());
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "target" | "result" | "node_modules" | ".git" | ".direnv"
                )
            {
                continue;
            }
            walk(&path, depth + 1, out);
        }
    }
    if root.is_dir() {
        walk(root, 0, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn key_from_path(path: &gtk::TreePath) -> String {
    path.indices()
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(":")
}

fn spawn<F: FnOnce(Tx) + Send + 'static>(tx: &Tx, work: F) {
    let tx = tx.clone();
    std::thread::spawn(move || work(tx));
}

// ---------------------------------------------------------------------------
// static scan -> tree
// ---------------------------------------------------------------------------

fn run_scan(ui: &Rc<Ui>, tx: &Tx) {
    let path = ui.flake_path.borrow().clone();
    if path.is_empty() {
        return;
    }
    ui.status.set_text("Scanning flake files...");
    spawn(tx, move |tx| {
        let result = extract::scan_repo(&path);
        let _ = tx.send_blocking(Msg::Scan(result));
    });
}

fn rebuild(ui: &Rc<Ui>, records: &[Record], targets: &[String]) {
    let _suppress = Suppress::new(&ui.suppress);
    ui.store.clear();
    ui.cstore.clear();
    ui.tree_map.borrow_mut().clear();
    ui.tree_rows.borrow_mut().clear();
    ui.view_map.borrow_mut().clear();
    *ui.levels.borrow_mut() = model::build_levels(records);
    *ui.targets.borrow_mut() = targets.to_vec();

    ui.right_path.borrow_mut().clear();
    refresh_tree(ui);
    refresh_right(ui);
    *ui.current.borrow_mut() = None;
    close_popup(ui);

    let keys = records.len();
    ui.status
        .set_text(&format!("{keys} configured option(s) from flake files"));
}

fn refresh_right(ui: &Rc<Ui>) {
    let _suppress = Suppress::new(&ui.suppress);
    let key = {
        let path = ui.right_path.borrow();
        path.join(".")
    };
    ui.crumb
        .set_text(if key.is_empty() { "(root)" } else { &key });
    ui.cstore.clear();
    {
        let mut map = ui.view_map.borrow_mut();
        map.clear();
        let Some(nodes) = ui.levels.borrow().get(&key).cloned() else {
            return;
        };
        let query = ui.search.text().to_lowercase();
        let mut items: Vec<Node> = nodes;
        if !query.is_empty() {
            items.retain(|n| {
                n.name.to_lowercase().contains(&query)
                    || n.path.to_lowercase().contains(&query)
            });
        }
        items.sort_by(|a, b| {
            let af = a.is_folder();
            let bf = b.is_folder();
            bf.cmp(&af).then_with(|| a.name.cmp(&b.name))
        });
        for node in items.iter() {
            let row = ui.cstore.append(None);
            let icon = if node.is_folder() {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            };
            let ty = match node.records.first() {
                Some(r) if !r.origin.is_empty() => {
                    format!("{} · from {}", node.short_type(), r.origin)
                }
                _ => node.short_type().to_string(),
            };
            ui.cstore.set(
                &row,
                &[
                    (C_NAME, &node.name as &dyn glib::value::ToValue),
                    (C_TYPE, &ty as &dyn glib::value::ToValue),
                    (C_ICON, &icon as &dyn glib::value::ToValue),
                ],
            );
            map.insert(key_from_path(&ui.cstore.path(&row)), node.clone());
        }
    }
}

/// Populate the left pane with every namespace from the scan, recursively.
fn refresh_tree(ui: &Rc<Ui>) {
    let _suppress = Suppress::new(&ui.suppress);
    add_namespaces(ui, None, "");
}

fn add_namespaces(ui: &Rc<Ui>, parent: Option<&gtk::TreeIter>, parent_key: &str) {
    let Some(nodes) = ui.levels.borrow().get(parent_key).cloned() else {
        return;
    };
    let mut folders: Vec<Node> = nodes
        .into_iter()
        .filter(|n| n.is_folder())
        .collect();
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    for node in folders {
        let row = ui.store.append(parent);
        ui.store.set(
            &row,
            &[
                (T_NAME, &node.name as &dyn glib::value::ToValue),
                (T_ICON, &"folder-symbolic" as &dyn glib::value::ToValue),
            ],
        );
        let rkey = key_from_path(&ui.store.path(&row));
        ui.tree_map.borrow_mut().insert(rkey.clone(), node.clone());
        ui.tree_rows
            .borrow_mut()
            .insert(node.path.clone(), rkey);
        add_namespaces(ui, Some(&row), &node.path);
    }
}

// ---------------------------------------------------------------------------
// detail popup
// ---------------------------------------------------------------------------

fn open_popup(ui: &Rc<Ui>, tx: &Tx, node: &Node) {
    close_popup(ui);
    let dialog = gtk::Dialog::builder()
        .title(&node.name)
        .transient_for(&ui.window)
        .modal(true)
        .build();
    let detail = build_detail();
    dialog.set_default_size(820, 680);
    dialog.content_area().append(&detail.root);

    {
        let ui = ui.clone();
        dialog.connect_close_request(move |_| {
            *ui.popup.borrow_mut() = None;
            ui.popup_path.borrow_mut().clear();
            ui.current.borrow_mut().take();
            glib::Propagation::Proceed
        });
    }

    wire_popup_actions(ui, tx, &detail);
    dialog.present();

    populate_detail(&detail, &ui.flake_path.borrow(), node);
    *ui.popup_path.borrow_mut() = node.path.clone();
    *ui.current.borrow_mut() = Some(node.clone());
    *ui.popup.borrow_mut() = Some((dialog, detail));
    request_nixd(ui, tx, node);
}

/// Query nixd for the option's type/documentation as soon as the popup opens.
fn request_nixd(ui: &Rc<Ui>, tx: &Tx, node: &Node) {
    let path = ui.flake_path.borrow().clone();
    let (target, option_path) = nixd_args(&node.segments, &ui.targets.borrow());
    spawn(tx, move |tx| {
        let result = nixd::query_hover(&path, &target, &option_path);
        let _ = tx.send_blocking(Msg::Nixd(result));
    });
}

fn wire_popup_actions(ui: &Rc<Ui>, tx: &Tx, detail: &Detail) {
    detail.ok_btn.clone().connect_clicked({
        let ui = ui.clone();
        let tx = tx.clone();
        move |_| {
            let edits = {
                let popup = ui.popup.borrow();
                let Some((_, detail)) = popup.as_ref() else {
                    return;
                };
                let root_path = ui.flake_path.borrow().clone();
                let sources = detail.sources.borrow();
                let mut edits: Vec<(String, usize, usize, String)> = Vec::new();
                for source in sources.iter() {
                    if source.editor.unchanged(&source.original) {
                        continue;
                    }
                    if !edit::editable(&source.record.file, &root_path) {
                        continue;
                    }
                    edits.push((
                        source.record.file.clone(),
                        source.record.byte_start,
                        source.record.byte_end,
                        source.editor.to_nix(),
                    ));
                }
                edits
            };

            if edits.is_empty() {
                close_popup(&ui);
                return;
            }

            ui.status.set_text("Applying change...");
            spawn(&tx, move |tx| {
                let mut by_file: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
                for (file, start, end, value) in edits {
                    by_file.entry(file).or_default().push((start, end, value));
                }
                let mut result = Ok(());
                for (file, spans) in by_file {
                    if let Err(e) = edit::set_values_in_file(&file, &spans) {
                        result = Err(e);
                        break;
                    }
                }
                let _ = tx.send_blocking(Msg::Edited(result.map(|_| String::new())));
            });
        }
    });

    detail.cancel_btn.clone().connect_clicked({
        let ui = ui.clone();
        move |_| close_popup(&ui)
    });
}

/// Map a node's output-prefixed path to a nixd target plus the bare option
/// path nixd expects inside it.
fn nixd_args(segments: &[String], targets: &[String]) -> (String, String) {
    let fallback = targets.first().cloned().unwrap_or_default();
    match segments.first().map(String::as_str) {
        Some("nixosConfigurations") if segments.len() >= 2 => (
            format!("nixos:{}", segments[1]),
            segments[2..].join("."),
        ),
        Some("homeConfigurations") if segments.len() >= 2 => {
            (format!("home:{}", segments[1]), segments[2..].join("."))
        }
        Some("darwinConfigurations") if segments.len() >= 2 => {
            (format!("darwin:{}", segments[1]), segments[2..].join("."))
        }
        Some("modules") if segments.len() >= 4 => (fallback, segments[3..].join(".")),
        _ => (fallback, segments.join(".")),
    }
}

fn close_popup(ui: &Ui) {
    let popup = ui.popup.borrow_mut().take();
    if let Some((dialog, _)) = popup {
        let _ = dialog.close();
    }
}

// ---------------------------------------------------------------------------
// detail widgets
// ---------------------------------------------------------------------------

struct Detail {
    root: gtk::Box,
    name: gtk::Label,
    path: gtk::Label,
    ty: gtk::Label,
    locations: gtk::Box,
    sources: RefCell<Vec<SourceEditor>>,
    info: gtk::Label,
    ok_btn: gtk::Button,
    cancel_btn: gtk::Button,
}

/// One editable source of a key: the record it was found in, the live editor,
/// and the value it started with (so unchanged sources are skipped on save).
struct SourceEditor {
    block: gtk::Box,
    record: Record,
    editor: Editor,
    original: String,
    kind: ValueKind,
}

/// The active value editor, chosen from the option's type.
enum Editor {
    Text {
        view: gtk::TextView,
        root: gtk::ScrolledWindow,
    },
    Bool(gtk::Switch),
    Number(gtk::SpinButton),
    List {
        root: gtk::Box,
        entries: Rc<RefCell<Vec<gtk::Entry>>>,
    },
}

impl Editor {
    fn root(&self) -> gtk::Widget {
        match self {
            Editor::Text { root, .. } => root.clone().upcast(),
            Editor::Bool(sw) => sw.clone().upcast(),
            Editor::Number(spin) => spin.clone().upcast(),
            Editor::List { root, .. } => root.clone().upcast(),
        }
    }

    /// Render the editor's current state back to Nix source.
    fn to_nix(&self) -> String {
        match self {
            Editor::Text { view, .. } => {
                let buffer = view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            }
            Editor::Bool(sw) => {
                if sw.is_active() {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            Editor::Number(spin) => {
                if spin.digits() == 0 {
                    format!("{}", spin.value() as i64)
                } else {
                    format!("{}", spin.value())
                }
            }
            Editor::List { entries, .. } => {
                let items: Vec<String> = entries
                    .borrow()
                    .iter()
                    .map(|e| e.text().to_string())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                value::serialize_list(&items)
            }
        }
    }

    /// Whether the editor still holds the original value, ignoring formatting
    /// differences (so an unchanged list does not trigger a rewrite).
    fn unchanged(&self, original: &str) -> bool {
        match self {
            Editor::List { entries, .. } => {
                let current: Vec<String> = entries
                    .borrow()
                    .iter()
                    .map(|e| e.text().trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                let orig: Vec<String> = value::list_items(original)
                    .unwrap_or_default()
                    .iter()
                    .map(|s| s.trim().to_string())
                    .collect();
                current == orig
            }
            Editor::Bool(sw) => original.trim() == if sw.is_active() { "true" } else { "false" },
            Editor::Number(spin) => original
                .trim()
                .parse::<f64>()
                .map(|v| v == spin.value())
                .unwrap_or(false),
            Editor::Text { .. } => self.to_nix().trim() == original.trim(),
        }
    }
}

/// Build a labelled, editable block for one source record: the file location
/// (and origin, when the key was imported) above an editor for that value.
fn build_source(record: &Record, editable: bool) -> SourceEditor {
    let block = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let mut text = format!("{}:{}:{}", record.file, record.line, record.col);
    if !record.origin.is_empty() {
        text.push_str(&format!("  ·  from {}", record.origin));
    }
    let header = gtk::Label::builder()
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .css_classes(["monospace", "dim-label"])
        .label(text)
        .build();
    block.append(&header);
    let kind = value::classify(&record.value);
    let editor = build_editor(kind, &record.value);
    editor.root().set_sensitive(editable);
    block.append(&editor.root());
    SourceEditor {
        block,
        record: record.clone(),
        editor,
        original: record.value.clone(),
        kind,
    }
}

/// Swap the editor of an existing source in place, keeping its header.
fn replace_source_editor(detail: &Detail, index: usize, kind: ValueKind) {
    let mut sources = detail.sources.borrow_mut();
    let Some(source) = sources.get_mut(index) else {
        return;
    };
    if source.editor.root().parent().is_some() {
        source.block.remove(&source.editor.root());
    }
    let editor = build_editor(kind, &source.original);
    source.block.append(&editor.root());
    source.editor = editor;
    source.kind = kind;
}

fn build_editor(kind: ValueKind, value_text: &str) -> Editor {
    match kind {
        ValueKind::Bool => {
            let sw = gtk::Switch::new();
            sw.set_active(value_text.trim() == "true");
            sw.set_halign(gtk::Align::Start);
            Editor::Bool(sw)
        }
        ValueKind::Int => build_number_editor(value_text, 0),
        ValueKind::Float => build_number_editor(value_text, 2),
        ValueKind::List => build_list_editor(value_text),
        _ => build_text_editor(value_text),
    }
}

fn build_text_editor(value_text: &str) -> Editor {
    let view = gtk::TextView::new();
    view.buffer().set_text(value_text);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(4);
    view.set_bottom_margin(4);
    view.set_left_margin(4);
    view.set_right_margin(4);
    let root = gtk::ScrolledWindow::builder()
        .child(&view)
        .min_content_height(140)
        .has_frame(true)
        .build();
    Editor::Text { view, root }
}

fn build_number_editor(value_text: &str, digits: u32) -> Editor {
    let spin = gtk::SpinButton::with_range(-1.0e15, 1.0e15, 1.0);
    spin.set_digits(digits);
    spin.set_value(value_text.trim().parse::<f64>().unwrap_or(0.0));
    spin.set_halign(gtk::Align::Start);
    Editor::Number(spin)
}

fn build_list_editor(value_text: &str) -> Editor {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let entries: Rc<RefCell<Vec<gtk::Entry>>> = Rc::new(RefCell::new(Vec::new()));

    let add = gtk::Button::with_label("Add item");
    {
        let root = root.clone();
        let entries = entries.clone();
        add.connect_clicked(move |_| add_list_row(&root, &entries, ""));
    }
    root.append(&add);

    for item in value::list_items(value_text).unwrap_or_default() {
        add_list_row(&root, &entries, &item);
    }

    Editor::List { root, entries }
}

fn add_list_row(root: &gtk::Box, entries: &Rc<RefCell<Vec<gtk::Entry>>>, text: &str) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let entry = gtk::Entry::new();
    entry.set_text(text);
    entry.set_hexpand(true);
    let remove = gtk::Button::with_label("Remove");
    {
        let root = root.clone();
        let entries = entries.clone();
        let row = row.clone();
        let entry = entry.clone();
        remove.connect_clicked(move |_| {
            root.remove(&row);
            entries.borrow_mut().retain(|e| *e != entry);
        });
    }
    row.append(&entry);
    row.append(&remove);
    root.append(&row);
    entries.borrow_mut().push(entry);
}

fn build_detail() -> Detail {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);
    root.set_width_request(760);

    let name = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-3"])
        .build();
    let path = gtk::Label::builder().xalign(0.0).selectable(true).wrap(true).build();
    let ty = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();

    let locations = gtk::Box::new(gtk::Orientation::Vertical, 10);
    locations.set_hexpand(true);
    let loc_scroll = gtk::ScrolledWindow::builder()
        .child(&locations)
        .vexpand(true)
        .propagate_natural_height(true)
        .min_content_height(120)
        .max_content_height(600)
        .build();

    let info = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    let info_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .hexpand(true)
        .child(&info)
        .reveal_child(false)
        .build();
    let info_btn = gtk::ToggleButton::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Show nixd type and description")
        .build();
    info_btn.add_css_class("flat");
    info_btn.connect_toggled({
        let revealer = info_revealer.clone();
        move |btn| revealer.set_reveal_child(btn.is_active())
    });
    let info_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    info_row.append(&info_btn);
    info_row.append(&info_revealer);

    let ok_btn = gtk::Button::with_label("OK");
    let cancel_btn = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel_btn);
    buttons.append(&ok_btn);

    root.append(&name);
    root.append(&path);
    root.append(&ty);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&gtk::Label::new(Some("Sources")));
    root.append(&loc_scroll);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&info_row);
    root.append(&buttons);

    Detail {
        root,
        name,
        path,
        ty,
        locations,
        sources: RefCell::new(Vec::new()),
        info,
        ok_btn,
        cancel_btn,
    }
}

fn populate_detail(detail: &Detail, root: &str, node: &Node) {
    detail.name.set_text(&node.name);
    detail.path.set_text(&node.path);

    while let Some(child) = detail.locations.first_child() {
        detail.locations.remove(&child);
    }
    let mut sources = Vec::new();
    for record in &node.records {
        let editable = edit::editable(&record.file, root);
        let source = build_source(record, editable);
        detail.locations.append(&source.block);
        sources.push(source);
    }
    let can_edit = node.records.iter().any(|r| edit::editable(&r.file, root));
    *detail.sources.borrow_mut() = sources;
    detail.ok_btn.set_sensitive(can_edit);

    let value_text = node.value.clone().unwrap_or_default();
    let kind = value::classify(&value_text);
    let mut ty = format!("{} · {}", node.short_type(), kind.label());
    if let Some(r) = node.records.first() {
        if !r.origin.is_empty() {
            ty.push_str(&format!("\nimported from {}", r.origin));
        }
        if !r.refs.is_empty() {
            ty.push_str(&format!("\nrefs: {}", r.refs.join(", ")));
        }
    }
    detail.ty.set_text(&ty);
    detail.info.set_text("Querying nixd...");
}

// ---------------------------------------------------------------------------
// widgets
// ---------------------------------------------------------------------------

fn build_ui(app: &adw::Application, tx: &Tx) -> Rc<Ui> {
    // left tree: folders only
    let store = gtk::TreeStore::new(&[glib::Type::STRING, glib::Type::STRING]);
    let tree = gtk::TreeView::builder().model(&store).headers_visible(true).build();
    add_column(&tree, "Registry", T_NAME, T_ICON as i32, true);

    // right contents
    let cstore = gtk::TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::STRING,
    ]);
    let view = gtk::TreeView::builder().model(&cstore).headers_visible(true).build();
    add_column(&view, "Name", C_NAME, C_ICON as i32, true);
    add_column(&view, "Type", C_TYPE, -1, false);

    let up_btn = gtk::Button::with_label("Up");
    let crumb = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    let bread = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bread.set_margin_top(6);
    bread.set_margin_bottom(6);
    bread.append(&up_btn);
    bread.append(&crumb);

    let view_scroll = gtk::ScrolledWindow::builder().child(&view).hexpand(true).vexpand(true).build();
    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&bread);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&view_scroll);

    let flake_combo = gtk::ComboBoxText::new();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Filter configured keys")
        .hexpand(true)
        .build();

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toolbar.set_margin_top(8);
    toolbar.set_margin_bottom(8);
    toolbar.set_margin_start(8);
    toolbar.set_margin_end(8);
    toolbar.append(&gtk::Label::new(Some("Flake")));
    toolbar.append(&flake_combo);
    toolbar.append(&search);

    let tree_scroll = gtk::ScrolledWindow::builder()
        .child(&tree)
        .hexpand(true)
        .vexpand(true)
        .build();

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&tree_scroll));
    paned.set_end_child(Some(&right));
    paned.set_position(320);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_vexpand(true);

    let status = gtk::Label::builder()
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&paned);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&status);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("nix-regedit")
        .default_width(1200)
        .default_height(760)
        .content(&root)
        .build();

    let ui = Rc::new(Ui {
        window,
        flake_combo,
        search,
        tree,
        store,
        up_btn,
        crumb,
        view,
        cstore,
        status,
        flakes: RefCell::new(Vec::new()),
        flake_path: RefCell::new(String::new()),
        targets: RefCell::new(Vec::new()),
        levels: RefCell::new(HashMap::new()),
        tree_map: RefCell::new(HashMap::new()),
        tree_rows: RefCell::new(HashMap::new()),
        right_path: RefCell::new(Vec::new()),
        view_map: RefCell::new(HashMap::new()),
        popup_path: RefCell::new(String::new()),
        current: RefCell::new(None),
        popup: RefCell::new(None),
        suppress: Cell::new(false),
    });

    wire(&ui, tx);
    ui
}

fn add_column(tree: &gtk::TreeView, title: &str, text_col: u32, icon_col: i32, expand: bool) {
    let col = gtk::TreeViewColumn::new();
    col.set_title(title);
    if icon_col >= 0 {
        let pix = gtk::CellRendererPixbuf::new();
        col.pack_start(&pix, false);
        col.add_attribute(&pix, "icon-name", icon_col);
    }
    let cell = gtk::CellRendererText::new();
    cell.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
    col.pack_start(&cell, true);
    col.add_attribute(&cell, "text", text_col as i32);
    if expand {
        col.set_expand(true);
    }
    tree.append_column(&col);
}

fn populate_flakes(ui: &Ui) {
    let root = dirs_home().join("repos");
    let flakes = discover_flakes(&root);
    ui.flake_combo.remove_all();
    {
        let mut store = ui.flakes.borrow_mut();
        store.clear();
        for f in flakes {
            ui.flake_combo
                .append(Some(&f.display().to_string()), &f.display().to_string());
            store.push(f);
        }
    }
    if !ui.flakes.borrow().is_empty() {
        let dendrite = ui
            .flakes
            .borrow()
            .iter()
            .position(|f| f.ends_with("nixos-configs/dendrite"))
            .unwrap_or(0);
        ui.flake_combo.set_active(Some(dendrite as u32));
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn wire(ui: &Rc<Ui>, tx0: &Tx) {
    // flake selection -> static scan of the whole repo
    {
        let ui = ui.clone();
        let tx = tx0.clone();
        ui.flake_combo.clone().connect_changed(move |combo| {
            if let Some(idx) = combo.active() {
                let path = ui
                    .flakes
                    .borrow()
                    .get(idx as usize)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                *ui.flake_path.borrow_mut() = path;
                run_scan(&ui, &tx);
            }
        });
    }

    // search filters the contents pane
    {
        let ui = ui.clone();
        ui.search.clone().connect_search_changed(move |_| {
            refresh_right(&ui);
        });
    }

    // left tree: selecting a folder shows its contents
    {
        let ui = ui.clone();
        let sel = ui.tree.selection();
        sel.connect_changed(move |sel| {
            if ui.suppress.get() {
                return;
            }
            let Some((_model, iter)) = sel.selected() else {
                return;
            };
            let key = key_from_path(&ui.store.path(&iter));
            let Some(node) = ui.tree_map.borrow().get(&key).cloned() else {
                return;
            };
            *ui.right_path.borrow_mut() = node.segments.clone();
            refresh_right(&ui);
        });
    }

    // contents pane: single click selects, double click opens (folder navigates,
    // key opens the detail popup)
    {
        let ui = ui.clone();
        let tx = tx0.clone();
        ui.view.set_activate_on_single_click(false);
        ui.view.clone().connect_row_activated(move |_, path, _| {
            if ui.suppress.get() {
                return;
            }
            let key = key_from_path(path);
            let Some(node) = ui.view_map.borrow().get(&key).cloned() else {
                return;
            };
            if node.is_folder() {
                *ui.right_path.borrow_mut() = node.segments.clone();
                refresh_right(&ui);
            } else if *ui.popup_path.borrow() != node.path {
                open_popup(&ui, &tx, &node);
            }
        });
    }

    // up button
    {
        let ui = ui.clone();
        ui.up_btn.clone().connect_clicked(move |_| {
            {
                let mut path = ui.right_path.borrow_mut();
                if path.is_empty() {
                    return;
                }
                path.pop();
            }
            refresh_right(&ui);
        });
    }
}

fn handle_msg(ui: &Rc<Ui>, tx: &Tx, msg: Msg) {
    match msg {
        Msg::Scan(Ok((records, targets))) => {
            rebuild(ui, &records, &targets);
        }
        Msg::Scan(Err(e)) => {
            ui.status.set_text("Flake scan failed");
            show_error(&ui.window, &e);
        }
        Msg::Edited(Ok(_)) => {
            ui.status.set_text("Saved, rescanning...");
            close_popup(&ui);
            run_scan(ui, tx);
        }
        Msg::Edited(Err(e)) => {
            ui.status.set_text("Edit failed");
            show_error(&ui.window, &e);
        }
        Msg::Nixd(Ok(text)) => {
            let popup = ui.popup.borrow();
            let Some((_, d)) = popup.as_ref() else {
                return;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                d.info.set_text("nixd returned no additional info");
                return;
            }
            d.info.set_text(trimmed);
            let declared = value::extract_type(trimmed).map(|t| value::kind_from_type_string(&t));
            if let Some(kind) = declared {
                if kind.is_concrete() {
                    let indexes: Vec<usize> = d
                        .sources
                        .borrow()
                        .iter()
                        .enumerate()
                        .filter(|(_, source)| {
                            source.kind != kind && source.editor.unchanged(&source.original)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    for index in indexes {
                        replace_source_editor(d, index, kind);
                    }
                }
            }
        }
        Msg::Nixd(Err(e)) => {
            if let Some((_, d)) = ui.popup.borrow().as_ref() {
                d.info.set_text(&format!("nixd: {e}"));
            }
        }
    }
}

fn show_error(parent: &impl IsA<gtk::Window>, message: &str) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(gtk::MessageType::Error)
        .buttons(gtk::ButtonsType::Close)
        .text(message)
        .build();
    dialog.connect_response(|d, _| d.close());
    dialog.present();
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    let (tx, rx) = async_channel::unbounded::<Msg>();
    let rx_cell = std::cell::Cell::new(Some(rx));

    app.connect_activate(move |app| {
        let ui = build_ui(app, &tx);
        if let Some(rx) = rx_cell.take() {
            let ui_for_rx = ui.clone();
            let tx_for_rx = tx.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(msg) = rx.recv().await {
                    handle_msg(&ui_for_rx, &tx_for_rx, msg);
                }
            });
        }
        populate_flakes(&ui);
        ui.window.present();
    });

    app.run()
}