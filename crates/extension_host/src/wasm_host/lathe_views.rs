//! Extension views: the validated form of `lathe:extension/views@0.2.0`.
//!
//! An extension sends a flat node list (WIT has no recursive types). This
//! checks it once, on arrival, so the renderer can walk it without bounds
//! checks or cycle guards.

use anyhow::{Result, bail};
use collections::HashSet;
use std::sync::Arc;

/// Most nodes a single view may have.
pub const MAX_NODES: usize = 2_000;
/// Deepest nesting of rows and columns.
pub const MAX_DEPTH: usize = 16;
/// Longest text, label or detail, in characters.
pub const MAX_TEXT_CHARS: usize = 2_000;
/// Longest view or element id, in characters.
pub const MAX_ID_CHARS: usize = 200;
/// Most views one extension may keep.
pub const MAX_VIEWS_PER_EXTENSION: usize = 32;
/// Deepest tree-view indent a list item may have.
pub const MAX_INDENT: u8 = 8;
/// Longest markdown node, in characters.
pub const MAX_MARKDOWN_CHARS: usize = 20_000;
/// Most entries in a list item's menu.
pub const MAX_MENU_ENTRIES: usize = 32;
/// Most options in a select.
pub const MAX_SELECT_OPTIONS: usize = 200;
/// Most rows a virtual list may have.
pub const MAX_VIRTUAL_ROWS: u32 = 1_000_000;
/// Most rows Lathe asks an extension for at once.
pub const ROWS_PER_REQUEST: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextStyle {
    Body,
    Muted,
    Heading,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disclosure {
    None,
    Collapsed,
    Expanded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: Arc<str>,
    pub label: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub value: Arc<str>,
    pub label: Arc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Column,
    Row,
    Scroll,
    Text {
        text: Arc<str>,
        style: TextStyle,
        tone: Tone,
    },
    Button {
        id: Arc<str>,
        label: Arc<str>,
        icon: Option<Arc<str>>,
        tooltip: Option<Arc<str>>,
        disabled: bool,
    },
    ListItem {
        id: Arc<str>,
        label: Arc<str>,
        detail: Option<Arc<str>>,
        icon: Option<Arc<str>>,
        tooltip: Option<Arc<str>>,
        selected: bool,
        disabled: bool,
        indent: u8,
        disclosure: Disclosure,
        menu: Vec<MenuEntry>,
    },
    TextInput {
        id: Arc<str>,
        value: Arc<str>,
        placeholder: Option<Arc<str>>,
        disabled: bool,
    },
    Checkbox {
        id: Arc<str>,
        label: Arc<str>,
        checked: bool,
        disabled: bool,
    },
    Badge {
        text: Arc<str>,
        tone: Tone,
    },
    Select {
        id: Arc<str>,
        options: Vec<SelectOption>,
        selected: Option<Arc<str>>,
        placeholder: Option<Arc<str>>,
        disabled: bool,
    },
    VirtualList {
        id: Arc<str>,
        row_count: u32,
        revision: u32,
    },
    /// `fraction` is in 0.0..=1.0; `None` is a spinner.
    Progress {
        fraction: Option<f32>,
        label: Option<Arc<str>>,
    },
    Markdown(Arc<str>),
    Icon(Arc<str>),
    Divider,
    Spacer,
}

impl NodeKind {
    fn is_container(&self) -> bool {
        matches!(self, Self::Column | Self::Row | Self::Scroll)
    }

    fn element_id(&self) -> Option<&Arc<str>> {
        match self {
            Self::Button { id, .. }
            | Self::ListItem { id, .. }
            | Self::TextInput { id, .. }
            | Self::Checkbox { id, .. }
            | Self::Select { id, .. }
            | Self::VirtualList { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// What the user did to an element of a view, as sent to the extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Clicked,
    Toggled(bool),
    InputChanged(String),
    Submitted(String),
    MenuChosen(String),
    Selected(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub children: Vec<u32>,
}

/// A view that passed [`validate`]: every index is in bounds, every node is
/// reachable from node 0 exactly once, and only rows and columns have
/// children.
#[derive(Debug, Clone, PartialEq)]
pub struct LatheView {
    nodes: Arc<[Node]>,
}

impl LatheView {
    pub fn root(&self) -> &Node {
        &self.nodes[0]
    }

    pub fn node(&self, index: u32) -> &Node {
        &self.nodes[index as usize]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Whether `element_id` names an interactive element in this view.
    pub fn has_element(&self, element_id: &str) -> bool {
        self.nodes.iter().any(|node| {
            node.kind
                .element_id()
                .is_some_and(|id| id.as_ref() == element_id)
        })
    }
}

pub fn validate_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("{kind} id must not be empty");
    }
    if id.chars().count() > MAX_ID_CHARS {
        bail!("{kind} id is longer than {MAX_ID_CHARS} characters");
    }
    Ok(())
}

fn validate_text(what: &str, text: &str) -> Result<()> {
    if text.chars().count() > MAX_TEXT_CHARS {
        bail!("{what} is longer than {MAX_TEXT_CHARS} characters");
    }
    Ok(())
}

fn validate_kind(kind: &NodeKind) -> Result<()> {
    match kind {
        NodeKind::Text { text, .. } => validate_text("text", text)?,
        NodeKind::Badge { text, .. } => validate_text("badge", text)?,
        NodeKind::Button {
            id, label, tooltip, ..
        } => {
            validate_id("button", id)?;
            validate_text("button label", label)?;
            validate_optional_text("button tooltip", tooltip)?;
        }
        NodeKind::ListItem {
            id,
            label,
            detail,
            tooltip,
            indent,
            menu,
            ..
        } => {
            validate_id("list item", id)?;
            validate_menu(menu)?;
            validate_text("list item label", label)?;
            validate_optional_text("list item detail", detail)?;
            validate_optional_text("list item tooltip", tooltip)?;
            if *indent > MAX_INDENT {
                bail!("list item indent can be at most {MAX_INDENT}, got {indent}");
            }
        }
        NodeKind::TextInput {
            id,
            value,
            placeholder,
            ..
        } => {
            validate_id("text input", id)?;
            validate_text("text input value", value)?;
            validate_optional_text("text input placeholder", placeholder)?;
        }
        NodeKind::Checkbox { id, label, .. } => {
            validate_id("checkbox", id)?;
            validate_text("checkbox label", label)?;
        }
        NodeKind::Select {
            id,
            options,
            selected,
            placeholder,
            ..
        } => {
            validate_id("select", id)?;
            validate_optional_text("select placeholder", placeholder)?;
            validate_select(options, selected.as_deref())?;
        }
        NodeKind::Progress { fraction, label } => {
            validate_optional_text("progress label", label)?;
            if let Some(fraction) = fraction
                && !(0.0..=1.0).contains(fraction)
            {
                bail!("progress fraction must be from 0.0 to 1.0, got {fraction}");
            }
        }
        NodeKind::VirtualList { id, row_count, .. } => {
            validate_id("virtual list", id)?;
            if *row_count > MAX_VIRTUAL_ROWS {
                bail!("a virtual list can have at most {MAX_VIRTUAL_ROWS} rows");
            }
        }
        NodeKind::Markdown(markdown) => {
            if markdown.chars().count() > MAX_MARKDOWN_CHARS {
                bail!("markdown is longer than {MAX_MARKDOWN_CHARS} characters");
            }
        }
        _ => {}
    }
    Ok(())
}

/// Checks rows an extension returned for a virtual list: list items only,
/// valid like any list item, ids unique within the page, and no more than
/// were asked for.
pub fn validate_rows(rows: &[NodeKind], requested: u32) -> Result<()> {
    if rows.len() > requested as usize {
        bail!("asked for {requested} rows, got {}", rows.len());
    }
    let mut ids = HashSet::default();
    for row in rows {
        let NodeKind::ListItem { id, .. } = row else {
            bail!("virtual list rows must be list items");
        };
        validate_kind(row)?;
        if !ids.insert(id.clone()) {
            bail!("row id {id:?} is used more than once");
        }
    }
    Ok(())
}

fn validate_menu(menu: &[MenuEntry]) -> Result<()> {
    if menu.len() > MAX_MENU_ENTRIES {
        bail!("a menu can have at most {MAX_MENU_ENTRIES} entries");
    }
    let mut ids = HashSet::default();
    for entry in menu {
        validate_id("menu entry", &entry.id)?;
        validate_text("menu entry label", &entry.label)?;
        if !ids.insert(entry.id.clone()) {
            bail!("menu entry id {:?} is used more than once", entry.id);
        }
    }
    Ok(())
}

fn validate_select(options: &[SelectOption], selected: Option<&str>) -> Result<()> {
    if options.len() > MAX_SELECT_OPTIONS {
        bail!("a select can have at most {MAX_SELECT_OPTIONS} options");
    }
    let mut values = HashSet::default();
    for option in options {
        validate_id("select option value", &option.value)?;
        validate_text("select option label", &option.label)?;
        if !values.insert(option.value.clone()) {
            bail!(
                "select option value {:?} is used more than once",
                option.value
            );
        }
    }
    if let Some(selected) = selected
        && !values.contains(selected)
    {
        bail!("selected value {selected:?} is not one of the select's options");
    }
    Ok(())
}

fn validate_optional_text(what: &str, text: &Option<Arc<str>>) -> Result<()> {
    match text {
        Some(text) => validate_text(what, text),
        None => Ok(()),
    }
}

pub fn validate(nodes: Vec<Node>) -> Result<LatheView> {
    if nodes.is_empty() {
        bail!("a view needs at least a root node");
    }
    if nodes.len() > MAX_NODES {
        bail!(
            "a view can have at most {MAX_NODES} nodes, got {}",
            nodes.len()
        );
    }

    let mut element_ids = HashSet::default();
    for (index, node) in nodes.iter().enumerate() {
        if !node.children.is_empty() && !node.kind.is_container() {
            bail!("node {index} has children, but only rows, columns and scrolls can");
        }
        validate_kind(&node.kind)?;
        if let Some(id) = node.kind.element_id()
            && !element_ids.insert(id.clone())
        {
            bail!("element id {id:?} is used more than once");
        }
    }

    // Walk from the root: each node reached exactly once, no deeper than
    // MAX_DEPTH. This rules out cycles and shared nodes.
    let mut seen = vec![false; nodes.len()];
    seen[0] = true;
    let mut stack = vec![(0_u32, 1_usize)];
    while let Some((index, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            bail!("views can nest at most {MAX_DEPTH} levels deep");
        }
        for &child in &nodes[index as usize].children {
            let Some(slot) = seen.get_mut(child as usize) else {
                bail!("node {index} lists child {child}, which doesn't exist");
            };
            if *slot {
                bail!("node {child} is reached more than once; each node needs exactly one parent");
            }
            *slot = true;
            stack.push((child, depth + 1));
        }
    }
    if let Some(orphan) = seen.iter().position(|seen| !seen) {
        bail!("node {orphan} isn't reachable from the root");
    }

    Ok(LatheView {
        nodes: nodes.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(kind: NodeKind, children: &[u32]) -> Node {
        Node {
            kind,
            children: children.to_vec(),
        }
    }

    fn leaf(kind: NodeKind) -> Node {
        Node {
            kind,
            children: Vec::new(),
        }
    }

    fn button(id: &str) -> NodeKind {
        NodeKind::Button {
            id: id.into(),
            label: "Go".into(),
            icon: None,
            tooltip: None,
            disabled: false,
        }
    }

    fn input(id: &str) -> NodeKind {
        NodeKind::TextInput {
            id: id.into(),
            value: "".into(),
            placeholder: None,
            disabled: false,
        }
    }

    fn list_item(id: &str, indent: u8) -> NodeKind {
        NodeKind::ListItem {
            id: id.into(),
            label: id.into(),
            detail: None,
            icon: None,
            tooltip: None,
            selected: false,
            disabled: false,
            indent,
            disclosure: Disclosure::None,
            menu: Vec::new(),
        }
    }

    fn error(nodes: Vec<Node>) -> String {
        format!("{:#}", validate(nodes).unwrap_err())
    }

    #[test]
    fn test_valid_view() {
        let view = validate(vec![
            container(NodeKind::Column, &[1, 2]),
            leaf(NodeKind::Text {
                text: "Hi".into(),
                style: TextStyle::Heading,
                tone: Tone::Neutral,
            }),
            container(NodeKind::Row, &[3]),
            leaf(button("go")),
        ])
        .unwrap();
        assert_eq!(view.len(), 4);
        assert!(view.has_element("go"));
        assert!(!view.has_element("missing"));
        assert_eq!(view.root().children, vec![1, 2]);
    }

    #[test]
    fn test_rejected_views() {
        assert!(error(vec![]).contains("root"));
        assert!(error(vec![container(NodeKind::Column, &[5])]).contains("doesn't exist"));
        assert!(
            error(vec![
                container(NodeKind::Column, &[1]),
                container(NodeKind::Row, &[0])
            ])
            .contains("more than once"),
            "cycle back to the root"
        );
        assert!(
            error(vec![
                container(NodeKind::Column, &[1, 2]),
                container(NodeKind::Row, &[3]),
                container(NodeKind::Row, &[3]),
                leaf(NodeKind::Divider),
            ])
            .contains("more than once"),
            "shared child"
        );
        assert!(
            error(vec![
                container(NodeKind::Column, &[]),
                leaf(NodeKind::Divider)
            ])
            .contains("isn't reachable")
        );
        assert!(
            error(vec![container(button("x"), &[0])]).contains("only rows, columns and scrolls")
        );
        assert!(
            error(vec![
                container(NodeKind::Column, &[1, 2]),
                leaf(button("same")),
                leaf(button("same"))
            ])
            .contains("more than once"),
            "duplicate element id"
        );
        assert!(error(vec![container(NodeKind::Column, &[1]), leaf(button(""))]).contains("empty"));
        assert!(
            error(vec![leaf(NodeKind::Text {
                text: "x".repeat(MAX_TEXT_CHARS + 1).into(),
                style: TextStyle::Body,
                tone: Tone::Neutral,
            })])
            .contains("longer than")
        );
    }

    #[test]
    fn test_inputs_scroll_and_indent() {
        assert!(
            error(vec![
                container(NodeKind::Column, &[1, 2]),
                leaf(input("x")),
                leaf(button("x")),
            ])
            .contains("more than once"),
            "an input and a button can't share an id"
        );
        assert!(
            error(vec![
                container(NodeKind::Column, &[1]),
                leaf(list_item("deep", MAX_INDENT + 1)),
            ])
            .contains("indent")
        );
        assert!(
            validate(vec![
                container(NodeKind::Scroll, &[1, 2]),
                leaf(list_item("shallow", MAX_INDENT)),
                leaf(input("filter")),
            ])
            .is_ok(),
            "a scroll can hold children"
        );
    }

    #[test]
    fn test_menus_selects_progress_and_markdown() {
        let column = |children: &[u32]| container(NodeKind::Column, children);
        let entry = |id: &str| MenuEntry {
            id: id.into(),
            label: id.into(),
        };
        let mut item = list_item("item", 0);
        if let NodeKind::ListItem { menu, .. } = &mut item {
            *menu = vec![entry("a"), entry("a")];
        }
        assert!(error(vec![column(&[1]), leaf(item)]).contains("menu entry id"));

        let select = |selected: Option<&str>| NodeKind::Select {
            id: "sort".into(),
            options: vec![SelectOption {
                value: "name".into(),
                label: "Name".into(),
            }],
            selected: selected.map(Into::into),
            placeholder: None,
            disabled: false,
        };
        assert!(validate(vec![column(&[1]), leaf(select(Some("name")))]).is_ok());
        assert!(error(vec![column(&[1]), leaf(select(Some("size")))]).contains("not one of"));
        assert!(
            error(vec![
                column(&[1, 2]),
                leaf(select(None)),
                leaf(button("sort"))
            ])
            .contains("more than once"),
            "a select's id is an element id"
        );

        let progress = |fraction: f32| NodeKind::Progress {
            fraction: Some(fraction),
            label: None,
        };
        assert!(validate(vec![column(&[1]), leaf(progress(1.0))]).is_ok());
        assert!(error(vec![column(&[1]), leaf(progress(1.5))]).contains("fraction"));
        assert!(error(vec![column(&[1]), leaf(progress(f32::NAN))]).contains("fraction"));

        assert!(
            error(vec![
                column(&[1]),
                leaf(NodeKind::Markdown(
                    "x".repeat(MAX_MARKDOWN_CHARS + 1).into()
                )),
            ])
            .contains("markdown")
        );
    }

    #[test]
    fn test_virtual_lists_and_rows() {
        let list = |row_count: u32| NodeKind::VirtualList {
            id: "files".into(),
            row_count,
            revision: 0,
        };
        let column = |children: &[u32]| container(NodeKind::Column, children);
        assert!(validate(vec![column(&[1]), leaf(list(MAX_VIRTUAL_ROWS))]).is_ok());
        assert!(error(vec![column(&[1]), leaf(list(MAX_VIRTUAL_ROWS + 1))]).contains("at most"));

        let rows = vec![list_item("a", 0), list_item("b", 1)];
        assert!(validate_rows(&rows, 2).is_ok());
        assert!(
            validate_rows(&rows, 1)
                .unwrap_err()
                .to_string()
                .contains("asked for 1")
        );
        assert!(
            validate_rows(&[list_item("a", 0), list_item("a", 0)], 2)
                .unwrap_err()
                .to_string()
                .contains("more than once")
        );
        assert!(validate_rows(&[NodeKind::Divider], 1).is_err());
        assert!(validate_rows(&[list_item("deep", MAX_INDENT + 1)], 1).is_err());
    }

    #[test]
    fn test_depth_and_size_limits() {
        let deep = (0..=MAX_DEPTH as u32)
            .map(|index| {
                let next = index + 1;
                if (next as usize) <= MAX_DEPTH {
                    container(NodeKind::Column, &[next])
                } else {
                    leaf(NodeKind::Divider)
                }
            })
            .collect::<Vec<_>>();
        assert!(error(deep).contains("levels deep"));

        let mut wide = vec![container(
            NodeKind::Column,
            &(1..=MAX_NODES as u32).collect::<Vec<_>>(),
        )];
        wide.extend((0..MAX_NODES).map(|_| leaf(NodeKind::Divider)));
        assert!(error(wide).contains("at most"));
    }
}
