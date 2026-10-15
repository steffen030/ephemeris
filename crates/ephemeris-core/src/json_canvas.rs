//! JSON Canvas 1.0 types (Obsidian-compatible `.canvas` files).
//!
//! Spec: <https://jsoncanvas.org/spec/1.0/>
//!
//! Native Canvas has no freehand ink nodes. Ephemeris stores strokes in
//! sidecars and maps text cards to `text` nodes (plus optional `file` PNG
//! siblings for ink fidelity on export).

use serde::{Deserialize, Serialize};

/// Top-level JSON Canvas document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JsonCanvas {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<CanvasNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<CanvasEdge>,
}

/// A node on the canvas (`text` | `file` | `link` | `group`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CanvasNode {
    Text(TextNode),
    File(FileNode),
    Link(LinkNode),
    Group(GroupNode),
}

impl CanvasNode {
    pub fn id(&self) -> &str {
        match self {
            Self::Text(n) => &n.id,
            Self::File(n) => &n.id,
            Self::Link(n) => &n.id,
            Self::Group(n) => &n.id,
        }
    }

    pub fn x(&self) -> i32 {
        match self {
            Self::Text(n) => n.x,
            Self::File(n) => n.x,
            Self::Link(n) => n.x,
            Self::Group(n) => n.x,
        }
    }

    pub fn y(&self) -> i32 {
        match self {
            Self::Text(n) => n.y,
            Self::File(n) => n.y,
            Self::Link(n) => n.y,
            Self::Group(n) => n.y,
        }
    }

    pub fn width(&self) -> i32 {
        match self {
            Self::Text(n) => n.width,
            Self::File(n) => n.width,
            Self::Link(n) => n.width,
            Self::Group(n) => n.width,
        }
    }

    pub fn height(&self) -> i32 {
        match self {
            Self::Text(n) => n.height,
            Self::File(n) => n.height,
            Self::Link(n) => n.height,
            Self::Group(n) => n.height,
        }
    }

    pub fn color(&self) -> Option<&CanvasColor> {
        match self {
            Self::Text(n) => n.color.as_ref(),
            Self::File(n) => n.color.as_ref(),
            Self::Link(n) => n.color.as_ref(),
            Self::Group(n) => n.color.as_ref(),
        }
    }

    pub fn set_size(&mut self, width: i32, height: i32) {
        let (w, h) = match self {
            Self::Text(n) => (&mut n.width, &mut n.height),
            Self::File(n) => (&mut n.width, &mut n.height),
            Self::Link(n) => (&mut n.width, &mut n.height),
            Self::Group(n) => (&mut n.width, &mut n.height),
        };
        *w = width;
        *h = height;
    }

    /// True when `other` lies entirely inside this node's bounds.
    pub fn contains(&self, other: &CanvasNode) -> bool {
        other.x() >= self.x()
            && other.y() >= self.y()
            && other.x() + other.width() <= self.x() + self.width()
            && other.y() + other.height() <= self.y() + self.height()
    }

    pub fn set_pos(&mut self, x: i32, y: i32) {
        match self {
            Self::Text(n) => {
                n.x = x;
                n.y = y;
            }
            Self::File(n) => {
                n.x = x;
                n.y = y;
            }
            Self::Link(n) => {
                n.x = x;
                n.y = y;
            }
            Self::Group(n) => {
                n.x = x;
                n.y = y;
            }
        }
    }
}

/// Shared geometry + optional color for every node type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeBase {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextNode {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileNode {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkNode {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupNode {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "backgroundStyle"
    )]
    pub background_style: Option<BackgroundStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
}

/// Background image fit for group nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundStyle {
    Cover,
    Ratio,
    Repeat,
}

/// Edge connecting two nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasEdge {
    pub id: String,
    pub from_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_side: Option<NodeSide>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_end: Option<EdgeEnd>,
    pub to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_side: Option<NodeSide>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_end: Option<EdgeEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CanvasColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Side of a node an edge attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeSide {
    Top,
    Right,
    Bottom,
    Left,
}

impl NodeSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
        }
    }
}

/// Arrow / none endpoint shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeEnd {
    None,
    Arrow,
}

/// Preset `"1"`–`"6"` or hex `#RRGGBB` (JSON Canvas `canvasColor`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasColor(pub String);

impl CanvasColor {
    pub fn preset(n: u8) -> Self {
        Self(n.to_string())
    }

    pub fn hex(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl JsonCanvas {
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Generate a 16-char hex id (Obsidian-style).
pub fn new_canvas_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix time with a simple LCG-ish scramble for uniqueness within a process.
    let mixed = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xA5A5_A5A5_A5A5_A5A5);
    format!("{mixed:016x}")[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
  "nodes": [
    {
      "id": "6f0ad84f44ce9c17",
      "type": "text",
      "x": 0,
      "y": 0,
      "width": 250,
      "height": 120,
      "text": "Hello **world**"
    },
    {
      "id": "aabbccddeeff0011",
      "type": "file",
      "x": 300,
      "y": 0,
      "width": 200,
      "height": 100,
      "file": "Notes/Meeting.md"
    },
    {
      "id": "1122334455667788",
      "type": "link",
      "x": 0,
      "y": 200,
      "width": 200,
      "height": 80,
      "url": "https://example.com"
    },
    {
      "id": "99aabbccddeeff00",
      "type": "group",
      "x": -40,
      "y": -40,
      "width": 600,
      "height": 400,
      "label": "Ideas"
    }
  ],
  "edges": [
    {
      "id": "edge000011112222",
      "fromNode": "6f0ad84f44ce9c17",
      "fromSide": "right",
      "toNode": "aabbccddeeff0011",
      "toSide": "left",
      "toEnd": "arrow"
    }
  ]
}"#;

    #[test]
    fn round_trip_fixture() {
        let doc = JsonCanvas::from_json(FIXTURE).expect("parse");
        assert_eq!(doc.nodes.len(), 4);
        assert_eq!(doc.edges.len(), 1);
        match &doc.nodes[0] {
            CanvasNode::Text(t) => {
                assert_eq!(t.id, "6f0ad84f44ce9c17");
                assert_eq!(t.text, "Hello **world**");
            }
            other => panic!("expected text, got {other:?}"),
        }
        match &doc.nodes[1] {
            CanvasNode::File(f) => assert_eq!(f.file, "Notes/Meeting.md"),
            other => panic!("expected file, got {other:?}"),
        }
        match &doc.nodes[2] {
            CanvasNode::Link(l) => assert_eq!(l.url, "https://example.com"),
            other => panic!("expected link, got {other:?}"),
        }
        match &doc.nodes[3] {
            CanvasNode::Group(g) => assert_eq!(g.label.as_deref(), Some("Ideas")),
            other => panic!("expected group, got {other:?}"),
        }
        assert_eq!(doc.edges[0].from_side, Some(NodeSide::Right));
        assert_eq!(doc.edges[0].to_side, Some(NodeSide::Left));

        let again = JsonCanvas::from_json(&doc.to_json_pretty().unwrap()).unwrap();
        assert_eq!(doc, again);
    }

    #[test]
    fn empty_canvas_ok() {
        let doc = JsonCanvas::from_json("{}").unwrap();
        assert!(doc.nodes.is_empty());
        assert!(doc.edges.is_empty());
    }

    #[test]
    fn new_id_is_16_hex() {
        let id = new_canvas_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn set_pos_moves_text_node() {
        let mut n = CanvasNode::Text(TextNode {
            id: "a".into(),
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            text: String::new(),
            color: None,
        });
        n.set_pos(40, 60);
        assert_eq!(n.x(), 40);
        assert_eq!(n.y(), 60);
    }
}
