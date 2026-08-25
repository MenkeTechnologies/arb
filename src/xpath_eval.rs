//! The XPath 1.0 data model and evaluator.
//!
//! Section numbers are from the W3C Recommendation of 16 November 1999,
//! <https://www.w3.org/TR/1999/REC-xpath-19991116/>.
//!
//! The tree comes from `scraper`/html5ever. XPath's data model (§5) is not quite
//! html5ever's, and the two differences are handled here rather than being left
//! to surprise the caller:
//!
//! * **A doctype is not an XPath node.** §5 lists exactly seven node types and
//!   doctype is not among them, so `count(//node())` must not see it.
//! * **A synthesized empty `<head>` is not in the document.** html5ever
//!   implements the HTML5 tree-construction algorithm, which ALWAYS creates a
//!   `head` element; libxml2's HTML parser creates one only when there is head
//!   content. Measured on the same three inputs: for `<a>X</a><p>P</p>` and for
//!   `<html><body><a>X</a></body></html>`, `xmllint --html --xpath
//!   'count(//head)'` answers `0`, and for
//!   `<html><head><title>T</title></head>…` it answers `1`. An empty synthesized
//!   `head` is therefore hidden and a `head` with content is kept, which makes
//!   the two trees agree on all three. Without this, `//*`, `//node()` and every
//!   positional predicate over them would silently disagree with the reference.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use ego_tree::NodeId;
use scraper::{ElementRef, Html};

use crate::xpath_syntax::{Axis, Expr, NodeTest, PathStart, Principal, RelOp, Step};

/// Rewrite `<![CDATA[…]]>` sections into escaped character data.
///
/// XPath 1.0 §5.7: "characters inside CDATA sections are treated as character
/// data" — a CDATA section is TEXT, not a node type of its own. html5ever
/// implements the HTML5 tree-construction algorithm, where CDATA is only legal
/// in foreign content (SVG/MathML) and is otherwise parsed as a bogus COMMENT,
/// so `<a><![CDATA[x<y&z]]></a>` lost its content entirely: `string(//a)` was
/// empty where `xmllint --xpath` answers `x<y&z`, and `count(//a/text())` was 0
/// against its 1. Silently dropping input is the failure this engine exists to
/// remove, so the section is turned into the text it denotes before parsing.
///
/// The rewrite is textual and deliberately literal-minded: `<![CDATA[` inside a
/// comment would also be rewritten. That matches how an XML parser reads the
/// same bytes, and the alternative — losing the content — is worse.
fn decode_cdata(src: &str) -> std::borrow::Cow<'_, str> {
    const OPEN: &str = "<![CDATA[";
    const CLOSE: &str = "]]>";
    if !src.contains(OPEN) {
        return std::borrow::Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        match after.find(CLOSE) {
            Some(j) => {
                out.push_str(&escape_text(&after[..j]));
                rest = &after[j + CLOSE.len()..];
            }
            // Unterminated: the rest of the document is character data, which is
            // what an XML parser would report as an error and a lenient HTML one
            // treats as text. Keeping it beats dropping it.
            None => {
                out.push_str(&escape_text(after));
                rest = "";
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// A hasher for the two `NodeId`-keyed tables below.
///
/// A `NodeId` is one `NonZeroUsize`, and the default `HashMap` hashes it with
/// SipHash-1-3. That showed up as the single largest cost in a `sample` profile
/// of `count(//tr)` over a 20MB document: `Doc::key` 520 samples, SipHash 488,
/// `hash_one::<NodeId>` 303 — together more than half the run, for what is a
/// lookup of an integer in a dense range.
///
/// This is the FxHash mix (one multiply and a rotate, as used by rustc), which
/// is not collision-resistant and does not need to be: the keys are tree node
/// ids from this process's own arena, never attacker-controlled.
#[derive(Default)]
struct NodeHasher(u64);

impl Hasher for NodeHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_u8(*b);
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.write_usize(n as usize);
    }
    fn write_usize(&mut self, n: usize) {
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(SEED);
    }
}

type NodeMap<V> = HashMap<NodeId, V, BuildHasherDefault<NodeHasher>>;

/// A node in the XPath data model.
///
/// Attributes are nodes in XPath (§5.3) but not in an html5ever tree, so an
/// attribute is addressed as (owning element, index in that element's attribute
/// list). The `deterministic` feature of `scraper` makes that list keep SOURCE
/// order, which is the order `xmllint` reports `@*` in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XNode {
    Tree(NodeId),
    Attr(NodeId, usize),
}

/// The seven node types of §5. `Namespace` never occurs for HTML input but is
/// carried so the namespace axis has a principal type to be empty of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Root,
    Element,
    Text,
    Attribute,
    Comment,
    Pi,
    Namespace,
}

/// A parsed document plus the indexes the evaluator needs.
pub struct Doc {
    html: Html,
    /// Document order, pre-order over the VISIBLE tree.
    order: NodeMap<usize>,
    /// Nodes outside the XPath data model: the doctype, and an empty `<head>`
    /// html5ever synthesized that the reference parser would not have created.
    hidden: HashSet<NodeId>,
    /// Every visible TREE node, in document order, built ONCE.
    ///
    /// `following`/`preceding` used to rebuild and re-sort this per context
    /// node, which is quadratic in the document: over a table of 400 `td`s
    /// `//td/following::td` took 0.17s and over 1600 it took 2.28s — 4x the
    /// nodes for 13x the time.
    ordered: Vec<XNode>,
    /// For each visible node, one past the index of its last descendant. A
    /// subtree is CONTIGUOUS in document order, so this turns both scanning
    /// axes into a slice: everything at or after `end` is `following`, and an
    /// ancestor of `n` is exactly a node before `n` whose `end` is past it.
    end: NodeMap<usize>,
}

impl Doc {
    pub fn parse(src: &str) -> Doc {
        let html = Html::parse_document(&decode_cdata(src));
        // html5ever inserts a `tbody` around a table's rows; libxml2 never
        // synthesizes one. Measured: `xmllint --html --xpath 'count(//tbody)'`
        // answers 0 for `<table><tr><td>1</td></tr></table>` and 1 for the same
        // markup written with an explicit `<tbody>`. So a `tbody` the SOURCE
        // never mentions is html5ever's and is hidden; one the author wrote is
        // real and is kept.
        let source_has_tbody = src.to_ascii_lowercase().contains("<tbody");
        let mut hidden = HashSet::new();
        for n in html.tree.nodes() {
            let hide = match n.value() {
                // §5 lists exactly seven node types and doctype is not one.
                scraper::node::Node::Doctype(_) => true,
                scraper::node::Node::Element(e) => match e.name() {
                    "head" => n.children().next().is_none(),
                    "tbody" => !source_has_tbody,
                    _ => false,
                },
                _ => false,
            };
            if hide {
                hidden.insert(n.id());
            }
        }
        let mut order = NodeMap::default();
        let mut stack = vec![html.tree.root().id()];
        // Pre-order DFS, children pushed in reverse so they pop in order.
        let mut next = 0usize;
        while let Some(id) = stack.pop() {
            // A HIDDEN node gets no position of its own, but its children are in
            // the document and must keep theirs — `continue`ing past the whole
            // subtree left a reparented `<tr>` with no index at all, so it sorted
            // after every real node and `//node()` came out in the wrong order.
            if !hidden.contains(&id) {
                order.insert(id, next);
                next += 1;
            }
            let node = html.tree.get(id).expect("id came from this tree");
            let kids: Vec<NodeId> = node.children().map(|c| c.id()).collect();
            for k in kids.into_iter().rev() {
                stack.push(k);
            }
        }
        let mut doc = Doc {
            html,
            order,
            hidden,
            ordered: Vec::new(),
            end: NodeMap::default(),
        };
        doc.ordered = doc.build_ordered();
        doc.end = doc.build_ends();
        doc
    }

    /// Every visible tree node, sorted into document order. Called once.
    fn build_ordered(&self) -> Vec<XNode> {
        let mut v: Vec<XNode> = self
            .order
            .keys()
            .filter(|id| self.visible(**id))
            .map(|id| XNode::Tree(*id))
            .collect();
        v.sort_by_cached_key(|m| self.key(*m));
        v
    }

    /// One past the last descendant index, per node. A node's descendants are
    /// the contiguous run that follows it, so the end is the first later index
    /// whose node is NOT a descendant — computed here by walking the ordered
    /// list once per node's own subtree rather than scanning the document.
    fn build_ends(&self) -> NodeMap<usize> {
        let mut end =
            NodeMap::with_capacity_and_hasher(self.ordered.len(), BuildHasherDefault::default());
        // Walk backwards: a node's end is its own index + 1, extended by the
        // end of its last visible child, which is already known.
        for (i, n) in self.ordered.iter().enumerate().rev() {
            let XNode::Tree(id) = *n else { continue };
            let mut e = i + 1;
            for c in self.children_of(*n) {
                if let XNode::Tree(cid) = c {
                    if let Some(ce) = end.get(&cid) {
                        e = e.max(*ce);
                    }
                }
            }
            end.insert(id, e);
        }
        end
    }

    fn index_of(&self, n: XNode) -> usize {
        match n {
            XNode::Tree(id) => *self.order.get(&id).unwrap_or(&0),
            XNode::Attr(id, _) => *self.order.get(&id).unwrap_or(&0),
        }
    }

    fn end_of(&self, n: XNode) -> usize {
        match n {
            XNode::Tree(id) => *self.end.get(&id).unwrap_or(&0),
            // An attribute has no descendants; `following` from it starts after
            // its OWNING element's subtree (§2.2 counts document order).
            XNode::Attr(id, _) => *self.end.get(&id).unwrap_or(&0),
        }
    }

    pub fn root(&self) -> XNode {
        XNode::Tree(self.html.tree.root().id())
    }

    fn visible(&self, id: NodeId) -> bool {
        !self.hidden.contains(&id)
    }

    /// Sort key: an element's attributes sit immediately after it and before its
    /// children (§5.3 leaves their relative order implementation-dependent;
    /// source order is what the reference reports).
    fn key(&self, n: XNode) -> (usize, usize) {
        match n {
            XNode::Tree(id) => (*self.order.get(&id).unwrap_or(&usize::MAX), 0),
            XNode::Attr(id, i) => (*self.order.get(&id).unwrap_or(&usize::MAX), i + 1),
        }
    }

    pub fn kind(&self, n: XNode) -> Kind {
        match n {
            XNode::Attr(..) => Kind::Attribute,
            XNode::Tree(id) => match self.html.tree.get(id).map(|n| n.value()) {
                Some(scraper::node::Node::Document | scraper::node::Node::Fragment) => Kind::Root,
                Some(scraper::node::Node::Element(_)) => Kind::Element,
                Some(scraper::node::Node::Text(_)) => Kind::Text,
                Some(scraper::node::Node::Comment(_)) => Kind::Comment,
                Some(scraper::node::Node::ProcessingInstruction(_)) => Kind::Pi,
                _ => Kind::Root,
            },
        }
    }

    /// §5: the expanded name of a node, or `None` for the types that have none.
    pub fn name(&self, n: XNode) -> Option<String> {
        match n {
            XNode::Attr(id, i) => self.attr_at(id, i).map(|(k, _)| k),
            XNode::Tree(id) => match self.html.tree.get(id).map(|n| n.value()) {
                Some(scraper::node::Node::Element(e)) => Some(e.name().to_string()),
                Some(scraper::node::Node::ProcessingInstruction(p)) => Some(p.target.to_string()),
                _ => None,
            },
        }
    }

    fn attr_at(&self, id: NodeId, i: usize) -> Option<(String, String)> {
        let n = self.html.tree.get(id)?;
        let e = n.value().as_element()?;
        e.attrs()
            .nth(i)
            .map(|(k, v)| (k.to_string(), v.to_string()))
    }

    /// §5's string-value, per node type.
    pub fn string_value(&self, n: XNode) -> String {
        match n {
            XNode::Attr(id, i) => self.attr_at(id, i).map(|(_, v)| v).unwrap_or_default(),
            XNode::Tree(id) => match self.html.tree.get(id).map(|x| x.value()) {
                // §5.7: a text node's string-value is its character data.
                Some(scraper::node::Node::Text(t)) => t.text.to_string(),
                Some(scraper::node::Node::Comment(c)) => c.comment.to_string(),
                Some(scraper::node::Node::ProcessingInstruction(p)) => p.data.to_string(),
                // §5.1/§5.2: for the root and for an element, the concatenation
                // of the string-values of all TEXT node descendants.
                _ => {
                    let mut s = String::new();
                    if let Some(node) = self.html.tree.get(id) {
                        for d in node.descendants() {
                            if !self.visible(d.id()) {
                                continue;
                            }
                            if let scraper::node::Node::Text(t) = d.value() {
                                s.push_str(&t.text);
                            }
                        }
                    }
                    s
                }
            },
        }
    }

    /// How a node is rendered as one output line. Elements serialize back to
    /// HTML (what `xmllint --xpath` prints for an element node); an ATTRIBUTE
    /// renders as its VALUE, which is arb's stream convention and the one
    /// normalization `scripts/jq_parity.sh` applies to the reference.
    pub fn render(&self, n: XNode) -> String {
        match n {
            XNode::Attr(..) => self.string_value(n),
            XNode::Tree(id) => match self.html.tree.get(id).map(|x| x.value()) {
                Some(scraper::node::Node::Element(_)) => {
                    // Fast path: with nothing hidden underneath, html5ever's own
                    // serializer is authoritative and byte-identical to what
                    // every currently-passing probe expects.
                    if self.subtree_is_clean(id) {
                        self.html
                            .tree
                            .get(id)
                            .and_then(ElementRef::wrap)
                            .map(|e| e.html())
                            .unwrap_or_default()
                    } else {
                        // A synthesized `head`/`tbody` is not in the data model,
                        // so it must not appear in the markup either — otherwise
                        // `//html` would print a `<head></head>` that `//head`
                        // says does not exist.
                        let mut out = String::new();
                        self.write_node(&mut out, id);
                        out
                    }
                }
                Some(scraper::node::Node::Comment(c)) => format!("<!--{}-->", c.comment),
                Some(scraper::node::Node::ProcessingInstruction(p)) => {
                    format!("<?{} {}?>", p.target, p.data)
                }
                _ => self.string_value(n),
            },
        }
    }

    /// Does the subtree rooted at `id` contain no hidden node? Then html5ever's
    /// serializer can be used as-is.
    fn subtree_is_clean(&self, id: NodeId) -> bool {
        match self.html.tree.get(id) {
            Some(n) => n.descendants().all(|d| self.visible(d.id())),
            None => true,
        }
    }

    /// Serialize a subtree the way html5ever does, minus the hidden nodes.
    /// Only reached for a subtree that contains one (a synthesized `head` or
    /// `tbody`); everything else takes the fast path above.
    fn write_node(&self, out: &mut String, id: NodeId) {
        if !self.visible(id) {
            // Not in the data model: emit its children in its place.
            if let Some(n) = self.html.tree.get(id) {
                for c in n.children() {
                    self.write_node(out, c.id());
                }
            }
            return;
        }
        let Some(node) = self.html.tree.get(id) else {
            return;
        };
        match node.value() {
            scraper::node::Node::Text(t) => out.push_str(&escape_text(&t.text)),
            scraper::node::Node::Comment(c) => {
                out.push_str("<!--");
                out.push_str(&c.comment);
                out.push_str("-->");
            }
            scraper::node::Node::ProcessingInstruction(p) => {
                out.push_str(&format!("<?{} {}?>", p.target, p.data));
            }
            scraper::node::Node::Element(e) => {
                out.push('<');
                out.push_str(e.name());
                for (k, v) in e.attrs() {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(v));
                    out.push('"');
                }
                out.push('>');
                if is_void(e.name()) {
                    return;
                }
                for c in node.children() {
                    self.write_node(out, c.id());
                }
                out.push_str("</");
                out.push_str(e.name());
                out.push('>');
            }
            _ => {
                for c in node.children() {
                    self.write_node(out, c.id());
                }
            }
        }
    }

    // ── axes (§2.2) ─────────────────────────────────────────────────────────

    fn parent_of(&self, n: XNode) -> Option<XNode> {
        match n {
            // §5.3: "the element is the parent of each of these attribute
            // nodes", even though an attribute is not a child of the element.
            XNode::Attr(id, _) => Some(XNode::Tree(id)),
            XNode::Tree(id) => {
                let mut cur = self.html.tree.get(id)?.parent()?;
                while !self.visible(cur.id()) {
                    cur = cur.parent()?;
                }
                Some(XNode::Tree(cur.id()))
            }
        }
    }

    fn children_of(&self, n: XNode) -> Vec<XNode> {
        match n {
            XNode::Attr(..) => Vec::new(),
            XNode::Tree(id) => {
                let Some(node) = self.html.tree.get(id) else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                for c in node.children() {
                    if self.visible(c.id()) {
                        out.push(XNode::Tree(c.id()));
                    } else {
                        // A hidden element is not in the data model at all, so
                        // its children belong to ITS parent — which is what makes
                        // a table's rows children of the `table` the way libxml2
                        // reports them.
                        out.extend(self.children_of(XNode::Tree(c.id())));
                    }
                }
                out
            }
        }
    }

    fn descendants_of(&self, n: XNode) -> Vec<XNode> {
        let mut out = Vec::new();
        let mut stack: Vec<XNode> = self.children_of(n).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            out.push(c);
            for k in self.children_of(c).into_iter().rev() {
                stack.push(k);
            }
        }
        out
    }

    fn ancestors_of(&self, n: XNode) -> Vec<XNode> {
        let mut out = Vec::new();
        let mut cur = n;
        while let Some(p) = self.parent_of(cur) {
            out.push(p);
            cur = p;
        }
        out
    }

    fn siblings(&self, n: XNode, following: bool) -> Vec<XNode> {
        let Some(p) = self.parent_of(n) else {
            return Vec::new();
        };
        // An attribute node has no siblings on these axes (§2.2 defines them
        // over children of the parent, and an attribute is not a child).
        if matches!(n, XNode::Attr(..)) {
            return Vec::new();
        }
        let kids = self.children_of(p);
        let Some(pos) = kids.iter().position(|k| *k == n) else {
            return Vec::new();
        };
        if following {
            kids[pos + 1..].to_vec()
        } else {
            // Reverse axis: nearest first.
            kids[..pos].iter().rev().copied().collect()
        }
    }

    /// §2.2: the `following` axis is every node after the context node in
    /// document order, EXCLUDING descendants, attributes and namespace nodes.
    fn following_of(&self, n: XNode) -> Vec<XNode> {
        // A subtree is CONTIGUOUS in document order, so "after n and not a
        // descendant of n" is exactly "at or after the end of n's subtree" —
        // one slice, no filtering and no per-node sort.
        self.ordered
            .get(self.end_of(n).min(self.ordered.len())..)
            .map(<[XNode]>::to_vec)
            .unwrap_or_default()
    }

    /// §2.2: `preceding` is every node before the context node in document
    /// order, EXCLUDING ancestors, attributes and namespace nodes. Reverse axis,
    /// so nearest first.
    fn preceding_of(&self, n: XNode) -> Vec<XNode> {
        // Everything before `n`, minus its ancestors — and an ancestor is
        // exactly a node that starts before `n` and whose subtree END is past
        // it, so no ancestor set has to be built. Reverse axis: nearest first.
        let i = self.index_of(n);
        let mut out: Vec<XNode> = self.ordered[..i.min(self.ordered.len())]
            .iter()
            .filter(|m| self.end_of(**m) <= i)
            .copied()
            .collect();
        out.reverse();
        out
    }

    /// Every visible TREE node (attributes excluded, per the two axes above),
    /// in document order. Public so the front-end can find a re-parsed
    /// fragment's first element.
    pub fn document_nodes(&self) -> Vec<XNode> {
        self.ordered.clone()
    }

    fn attributes_of(&self, n: XNode) -> Vec<XNode> {
        match n {
            XNode::Tree(id) => match self.html.tree.get(id).and_then(|x| x.value().as_element()) {
                Some(e) => (0..e.attrs().count()).map(|i| XNode::Attr(id, i)).collect(),
                None => Vec::new(),
            },
            XNode::Attr(..) => Vec::new(),
        }
    }

    /// Every node on `axis` from `n`, in the axis's own order (document order for
    /// a forward axis, reverse document order for a reverse axis).
    fn axis(&self, axis: Axis, n: XNode) -> Vec<XNode> {
        match axis {
            Axis::SelfAxis => vec![n],
            Axis::Child => self.children_of(n),
            Axis::Parent => self.parent_of(n).into_iter().collect(),
            Axis::Descendant => self.descendants_of(n),
            Axis::DescendantOrSelf => {
                let mut v = vec![n];
                v.extend(self.descendants_of(n));
                v
            }
            Axis::Ancestor => self.ancestors_of(n),
            Axis::AncestorOrSelf => {
                let mut v = vec![n];
                v.extend(self.ancestors_of(n));
                v
            }
            Axis::FollowingSibling => self.siblings(n, true),
            Axis::PrecedingSibling => self.siblings(n, false),
            Axis::Following => self.following_of(n),
            Axis::Preceding => self.preceding_of(n),
            Axis::Attribute => self.attributes_of(n),
            // HTML input carries no namespace declarations libxml2 reports
            // either, so this axis is legitimately empty rather than refused.
            Axis::Namespace => Vec::new(),
        }
    }

    /// §2.3 node test, against the axis's principal node type.
    fn passes(&self, test: &NodeTest, axis: Axis, n: XNode) -> bool {
        let kind = self.kind(n);
        match test {
            NodeTest::Node => true,
            NodeTest::Text => kind == Kind::Text,
            NodeTest::Comment => kind == Kind::Comment,
            NodeTest::Pi(target) => {
                kind == Kind::Pi
                    && match target {
                        None => true,
                        Some(t) => self.name(n).as_deref() == Some(t.as_str()),
                    }
            }
            NodeTest::Any => principal_kind(axis.principal()) == kind,
            NodeTest::AnyInPrefix(_) => {
                // HTML has no prefixed names to match; libxml2 reports none for
                // HTML input either, so this selects nothing rather than
                // pretending the prefix matched.
                false
            }
            NodeTest::Name(want) => {
                if principal_kind(axis.principal()) != kind {
                    return false;
                }
                match self.name(n) {
                    // HTML names are matched case-insensitively: html5ever
                    // lowercases tag names, and an author may write `//DIV`.
                    Some(got) => got.eq_ignore_ascii_case(want),
                    None => false,
                }
            }
        }
    }

    /// The `lang()` function of §4.3: the nearest `lang`/`xml:lang` attribute on
    /// the node or an ancestor, if any.
    fn lang_of(&self, n: XNode) -> Option<String> {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if let XNode::Tree(id) = c {
                if let Some(e) = self.html.tree.get(id).and_then(|x| x.value().as_element()) {
                    if let Some(v) = e.attr("xml:lang").or_else(|| e.attr("lang")) {
                        return Some(v.to_string());
                    }
                }
            }
            cur = self.parent_of(c);
        }
        None
    }
}

/// The HTML void elements, which have no end tag.
fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// html5ever's text escaping: `&`, `<`, `>` and a non-breaking space.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
    out
}

/// html5ever's attribute-value escaping: `&`, `"` and a non-breaking space.
fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
    out
}

fn principal_kind(p: Principal) -> Kind {
    match p {
        Principal::Element => Kind::Element,
        Principal::Attribute => Kind::Attribute,
        Principal::Namespace => Kind::Namespace,
    }
}

// ── values (§1, §4) ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Value {
    /// Always kept in document order with duplicates removed (§3.3: a node-set
    /// is a set, and `|` computes a union).
    NodeSet(Vec<XNode>),
    Str(String),
    Num(f64),
    Bool(bool),
}

/// §4.2 `string(number)`: no exponent, no trailing `.0`, and the three special
/// spellings XPath names explicitly.
pub fn fmt_number(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if v == 0.0 {
        // §4.2 names "0" for both zeroes; `-0` is not a spelling XPath produces.
        return "0".into();
    }
    if v == v.trunc() && v.abs() < 1e21 {
        return format!("{}", v as i64);
    }
    let s = format!("{v}");
    // Rust may render an exponent where XPath 1.0 requires a plain decimal.
    if s.contains('e') || s.contains('E') {
        let mut t = format!("{v:.17}");
        while t.contains('.') && (t.ends_with('0') || t.ends_with('.')) {
            t.pop();
        }
        return t;
    }
    s
}

impl Value {
    /// §4.3 `boolean()`.
    pub fn boolean(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::NodeSet(v) => !v.is_empty(),
        }
    }

    /// §4.2 `string()`. For a node-set this is the string-value of the node
    /// FIRST in document order, which is why node-sets are kept sorted.
    pub fn string(&self, doc: &Doc) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Num(n) => fmt_number(*n),
            Value::NodeSet(v) => v.first().map(|n| doc.string_value(*n)).unwrap_or_default(),
        }
    }

    /// §4.4 `number()`.
    pub fn number(&self, doc: &Doc) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Str(s) => str_to_num(s),
            Value::NodeSet(_) => str_to_num(&self.string(doc)),
        }
    }

    fn nodes(&self) -> Option<&[XNode]> {
        match self {
            Value::NodeSet(v) => Some(v),
            _ => None,
        }
    }
}

/// §4.4: a string converts to a number only if it is an optionally-signed
/// decimal with optional surrounding whitespace; anything else is NaN.
fn str_to_num(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return f64::NAN;
    }
    // Rust accepts `inf`, `NaN`, `1e5` and `+1`; XPath 1.0's Number production
    // accepts none of those, so the shape is checked before parsing.
    let body = t.strip_prefix('-').unwrap_or(t);
    let mut seen_dot = false;
    if body.is_empty() {
        return f64::NAN;
    }
    for c in body.chars() {
        match c {
            '0'..='9' => {}
            '.' if !seen_dot => seen_dot = true,
            _ => return f64::NAN,
        }
    }
    if body == "." {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

// ── evaluation ──────────────────────────────────────────────────────────────

/// The evaluation context of §1: a node, a position and a size.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    node: XNode,
    pos: usize,
    size: usize,
}

/// Evaluate `expr` with `start` as the context node.
pub fn eval(doc: &Doc, expr: &Expr, start: XNode) -> Result<Value, String> {
    let ctx = Ctx {
        node: start,
        pos: 1,
        size: 1,
    };
    ev(doc, expr, ctx)
}

fn ev(doc: &Doc, e: &Expr, c: Ctx) -> Result<Value, String> {
    Ok(match e {
        Expr::Number(n) => Value::Num(*n),
        Expr::Literal(s) => Value::Str(s.clone()),
        Expr::Or(a, b) => {
            // Short-circuit is required by §3.4 ("the right operand is not
            // evaluated if the left operand evaluates to true").
            Value::Bool(ev(doc, a, c)?.boolean() || ev(doc, b, c)?.boolean())
        }
        Expr::And(a, b) => Value::Bool(ev(doc, a, c)?.boolean() && ev(doc, b, c)?.boolean()),
        Expr::Eq(a, b) => Value::Bool(compare_eq(doc, &ev(doc, a, c)?, &ev(doc, b, c)?, true)),
        Expr::Ne(a, b) => Value::Bool(compare_eq(doc, &ev(doc, a, c)?, &ev(doc, b, c)?, false)),
        Expr::Rel(op, a, b) => Value::Bool(compare_rel(doc, *op, &ev(doc, a, c)?, &ev(doc, b, c)?)),
        Expr::Add(a, b) => Value::Num(ev(doc, a, c)?.number(doc) + ev(doc, b, c)?.number(doc)),
        Expr::Sub(a, b) => Value::Num(ev(doc, a, c)?.number(doc) - ev(doc, b, c)?.number(doc)),
        Expr::Mul(a, b) => Value::Num(ev(doc, a, c)?.number(doc) * ev(doc, b, c)?.number(doc)),
        Expr::Div(a, b) => Value::Num(ev(doc, a, c)?.number(doc) / ev(doc, b, c)?.number(doc)),
        Expr::Mod(a, b) => Value::Num(ev(doc, a, c)?.number(doc) % ev(doc, b, c)?.number(doc)),
        Expr::Neg(a) => Value::Num(-ev(doc, a, c)?.number(doc)),
        Expr::Union(a, b) => {
            let (x, y) = (ev(doc, a, c)?, ev(doc, b, c)?);
            let (xs, ys) = (
                x.nodes().ok_or("`|` needs a node-set on the left")?,
                y.nodes().ok_or("`|` needs a node-set on the right")?,
            );
            let mut v: Vec<XNode> = xs.iter().chain(ys.iter()).copied().collect();
            sort_unique(doc, &mut v);
            Value::NodeSet(v)
        }
        Expr::Call(name, args) => call(doc, name, args, c)?,
        Expr::Path(start, steps) => {
            let mut set = match start {
                PathStart::Root => vec![doc.root()],
                PathStart::Relative => vec![c.node],
                PathStart::Filter(base, preds) => {
                    let v = ev(doc, base, c)?;
                    let mut nodes = v
                        .nodes()
                        .ok_or("a path can only continue from a node-set")?
                        .to_vec();
                    for p in preds {
                        nodes = filter(doc, nodes, p, false)?;
                    }
                    nodes
                }
            };
            for s in steps {
                set = step(doc, &set, s)?;
            }
            Value::NodeSet(set)
        }
    })
}

/// Apply one location step to every node of the current set, unioning the
/// results (§2: "the node-set selected by the location step is the union of the
/// node-sets selected by the step from each node").
fn step(doc: &Doc, set: &[XNode], s: &Step) -> Result<Vec<XNode>, String> {
    let mut out: Vec<XNode> = Vec::new();
    for n in set {
        let mut hits: Vec<XNode> = doc
            .axis(s.axis, *n)
            .into_iter()
            .filter(|m| doc.passes(&s.test, s.axis, *m))
            .collect();
        // §2.4: predicates are applied in order, each to the result of the last,
        // and the PROXIMITY POSITION is counted along the axis — which for a
        // reverse axis is reverse document order. `hits` is already in axis
        // order, so position is simply the index.
        for p in &s.preds {
            hits = filter(doc, hits, p, s.axis.is_reverse())?;
        }
        out.extend(hits);
    }
    sort_unique(doc, &mut out);
    Ok(out)
}

/// §2.4: keep the nodes whose predicate is true. A NUMBER result is compared
/// against the proximity position, so `[1]` means `[position()=1]`.
fn filter(doc: &Doc, nodes: Vec<XNode>, pred: &Expr, _reverse: bool) -> Result<Vec<XNode>, String> {
    let size = nodes.len();
    let mut out = Vec::new();
    for (i, n) in nodes.into_iter().enumerate() {
        let c = Ctx {
            node: n,
            pos: i + 1,
            size,
        };
        let v = ev(doc, pred, c)?;
        let keep = match v {
            Value::Num(x) => x == (i + 1) as f64,
            other => other.boolean(),
        };
        if keep {
            out.push(n);
        }
    }
    Ok(out)
}

fn sort_unique(doc: &Doc, v: &mut Vec<XNode>) {
    // `sort_by_CACHED_key`: the key is a map lookup, and the plain `sort_by_key`
    // recomputes it on every COMPARISON — O(n log n) lookups to order n nodes.
    // This computes it once per element.
    v.sort_by_cached_key(|n| doc.key(*n));
    v.dedup();
}

/// §3.4's comparison rules for `=` and `!=`.
///
/// The node-set cases are EXISTENTIAL, which is the part that trips people up:
/// `//a[@href!='x']` is true when the element has SOME attribute that is not
/// `x`, not when its `href` differs.
fn compare_eq(doc: &Doc, a: &Value, b: &Value, want_eq: bool) -> bool {
    let test = |x: &str, y: &str| if want_eq { x == y } else { x != y };
    match (a, b) {
        (Value::NodeSet(xs), Value::NodeSet(ys)) => xs.iter().any(|x| {
            let sx = doc.string_value(*x);
            ys.iter().any(|y| test(&sx, &doc.string_value(*y)))
        }),
        (Value::NodeSet(xs), other) | (other, Value::NodeSet(xs)) => match other {
            // "If one object is a node-set and the other is a boolean, then the
            // comparison is true iff boolean(node-set) compares to the boolean."
            Value::Bool(b2) => {
                let lhs = !xs.is_empty();
                if want_eq {
                    lhs == *b2
                } else {
                    lhs != *b2
                }
            }
            Value::Num(n2) => xs.iter().any(|x| {
                let v = str_to_num(&doc.string_value(*x));
                if want_eq {
                    v == *n2
                } else {
                    v != *n2
                }
            }),
            _ => {
                let s2 = other.string(doc);
                xs.iter().any(|x| test(&doc.string_value(*x), &s2))
            }
        },
        // Neither is a node-set: boolean wins, then number, then string.
        _ => {
            if matches!(a, Value::Bool(_)) || matches!(b, Value::Bool(_)) {
                let (x, y) = (a.boolean(), b.boolean());
                if want_eq {
                    x == y
                } else {
                    x != y
                }
            } else if matches!(a, Value::Num(_)) || matches!(b, Value::Num(_)) {
                let (x, y) = (a.number(doc), b.number(doc));
                if want_eq {
                    x == y
                } else {
                    x != y
                }
            } else {
                test(&a.string(doc), &b.string(doc))
            }
        }
    }
}

/// §3.4: `<`, `<=`, `>`, `>=` always compare NUMBERS, and a node-set operand is
/// existential the same way.
fn compare_rel(doc: &Doc, op: RelOp, a: &Value, b: &Value) -> bool {
    let cmp = |x: f64, y: f64| match op {
        RelOp::Lt => x < y,
        RelOp::Gt => x > y,
        RelOp::Le => x <= y,
        RelOp::Ge => x >= y,
    };
    match (a, b) {
        (Value::NodeSet(xs), Value::NodeSet(ys)) => xs.iter().any(|x| {
            let vx = str_to_num(&doc.string_value(*x));
            ys.iter()
                .any(|y| cmp(vx, str_to_num(&doc.string_value(*y))))
        }),
        (Value::NodeSet(xs), other) => {
            let n = other.number(doc);
            xs.iter().any(|x| cmp(str_to_num(&doc.string_value(*x)), n))
        }
        (other, Value::NodeSet(ys)) => {
            let n = other.number(doc);
            ys.iter().any(|y| cmp(n, str_to_num(&doc.string_value(*y))))
        }
        _ => cmp(a.number(doc), b.number(doc)),
    }
}

// ── the core function library (§4) ──────────────────────────────────────────

fn arity(name: &str, args: &[Expr], lo: usize, hi: usize) -> Result<(), String> {
    if args.len() < lo || args.len() > hi {
        let want = if lo == hi {
            format!("{lo}")
        } else {
            format!("{lo} to {hi}")
        };
        return Err(format!(
            "`{name}()` takes {want} argument(s), got {}",
            args.len()
        ));
    }
    Ok(())
}

fn call(doc: &Doc, name: &str, args: &[Expr], c: Ctx) -> Result<Value, String> {
    // The argument helpers evaluate lazily so arity is reported before a type
    // error inside an argument the function would not have used.
    let num = |i: usize| -> Result<f64, String> { Ok(ev(doc, &args[i], c)?.number(doc)) };
    let stringy = |i: usize| -> Result<String, String> { Ok(ev(doc, &args[i], c)?.string(doc)) };
    // A string argument that defaults to the CONTEXT NODE when omitted (§4.2).
    let str_or_ctx = |i: usize| -> Result<String, String> {
        if args.len() > i {
            stringy(i)
        } else {
            Ok(doc.string_value(c.node))
        }
    };
    let nodeset = |i: usize| -> Result<Vec<XNode>, String> {
        match ev(doc, &args[i], c)? {
            Value::NodeSet(v) => Ok(v),
            _ => Err(format!("`{name}()` needs a node-set argument")),
        }
    };
    Ok(match name {
        // §4.1 node-set functions.
        "last" => {
            arity(name, args, 0, 0)?;
            Value::Num(c.size as f64)
        }
        "position" => {
            arity(name, args, 0, 0)?;
            Value::Num(c.pos as f64)
        }
        "count" => {
            arity(name, args, 1, 1)?;
            Value::Num(nodeset(0)?.len() as f64)
        }
        "id" => {
            arity(name, args, 1, 1)?;
            let v = ev(doc, &args[0], c)?;
            // §4.1: a node-set argument means "the union of id() over the
            // string-value of each node"; anything else is one whitespace-
            // separated list of IDs.
            let ids: Vec<String> = match &v {
                Value::NodeSet(ns) => ns
                    .iter()
                    .flat_map(|n| {
                        doc.string_value(*n)
                            .split_whitespace()
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .collect(),
                other => other
                    .string(doc)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
            };
            let mut out: Vec<XNode> = Vec::new();
            for n in doc.document_nodes() {
                if let XNode::Tree(tid) = n {
                    if let Some(e) = doc.html.tree.get(tid).and_then(|x| x.value().as_element()) {
                        if e.attr("id").is_some_and(|v| ids.iter().any(|i| i == v)) {
                            out.push(n);
                        }
                    }
                }
            }
            sort_unique(doc, &mut out);
            Value::NodeSet(out)
        }
        "local-name" | "name" => {
            arity(name, args, 0, 1)?;
            let n = if args.is_empty() {
                Some(c.node)
            } else {
                nodeset(0)?.first().copied()
            };
            Value::Str(n.and_then(|n| doc.name(n)).unwrap_or_default())
        }
        "namespace-uri" => {
            arity(name, args, 0, 1)?;
            // HTML input has no namespace URIs the reference reports either.
            if !args.is_empty() {
                nodeset(0)?;
            }
            Value::Str(String::new())
        }
        // §4.2 string functions.
        "string" => {
            arity(name, args, 0, 1)?;
            Value::Str(if args.is_empty() {
                doc.string_value(c.node)
            } else {
                stringy(0)?
            })
        }
        "concat" => {
            arity(name, args, 2, usize::MAX)?;
            let mut s = String::new();
            for i in 0..args.len() {
                s.push_str(&stringy(i)?);
            }
            Value::Str(s)
        }
        "starts-with" => {
            arity(name, args, 2, 2)?;
            Value::Bool(stringy(0)?.starts_with(&stringy(1)?))
        }
        "contains" => {
            arity(name, args, 2, 2)?;
            Value::Bool(stringy(0)?.contains(&stringy(1)?))
        }
        "substring-before" => {
            arity(name, args, 2, 2)?;
            let (h, n) = (stringy(0)?, stringy(1)?);
            Value::Str(match h.find(&n) {
                Some(i) => h[..i].to_string(),
                None => String::new(),
            })
        }
        "substring-after" => {
            arity(name, args, 2, 2)?;
            let (h, n) = (stringy(0)?, stringy(1)?);
            Value::Str(match h.find(&n) {
                Some(i) => h[i + n.len()..].to_string(),
                None => String::new(),
            })
        }
        "substring" => {
            arity(name, args, 2, 3)?;
            // §4.2: indices are 1-based and are ROUNDED, and the result is the
            // characters whose position p satisfies start <= p < start+len, so
            // `substring("12345", 1.5, 2.6)` is "234".
            let chars: Vec<char> = stringy(0)?.chars().collect();
            let start = num(1)?;
            let end = if args.len() > 2 {
                let l = num(2)?;
                if l.is_nan() || start.is_nan() {
                    f64::NAN
                } else {
                    round_half_up(start) + round_half_up(l)
                }
            } else {
                f64::INFINITY
            };
            let lo = if start.is_nan() {
                f64::NAN
            } else {
                round_half_up(start)
            };
            let mut s = String::new();
            for (i, ch) in chars.iter().enumerate() {
                let p = (i + 1) as f64;
                if p >= lo && p < end {
                    s.push(*ch);
                }
            }
            Value::Str(s)
        }
        "string-length" => {
            arity(name, args, 0, 1)?;
            Value::Num(str_or_ctx(0)?.chars().count() as f64)
        }
        "normalize-space" => {
            arity(name, args, 0, 1)?;
            Value::Str(
                str_or_ctx(0)?
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
        "translate" => {
            arity(name, args, 3, 3)?;
            let (s, from, to) = (stringy(0)?, stringy(1)?, stringy(2)?);
            let from: Vec<char> = from.chars().collect();
            let to: Vec<char> = to.chars().collect();
            let mut out = String::new();
            for ch in s.chars() {
                match from.iter().position(|f| *f == ch) {
                    // §4.2: a character with no replacement is REMOVED.
                    Some(i) => {
                        if let Some(r) = to.get(i) {
                            out.push(*r);
                        }
                    }
                    None => out.push(ch),
                }
            }
            Value::Str(out)
        }
        // §4.3 boolean functions.
        "boolean" => {
            arity(name, args, 1, 1)?;
            Value::Bool(ev(doc, &args[0], c)?.boolean())
        }
        "not" => {
            arity(name, args, 1, 1)?;
            Value::Bool(!ev(doc, &args[0], c)?.boolean())
        }
        "true" => {
            arity(name, args, 0, 0)?;
            Value::Bool(true)
        }
        "false" => {
            arity(name, args, 0, 0)?;
            Value::Bool(false)
        }
        "lang" => {
            arity(name, args, 1, 1)?;
            let want = stringy(0)?.to_ascii_lowercase();
            Value::Bool(match doc.lang_of(c.node) {
                // §4.3: true when the argument equals the language, or is a
                // prefix of it followed by `-` (a sublanguage).
                Some(have) => {
                    let have = have.to_ascii_lowercase();
                    have == want || have.starts_with(&format!("{want}-"))
                }
                None => false,
            })
        }
        // §4.4 number functions.
        "number" => {
            arity(name, args, 0, 1)?;
            Value::Num(if args.is_empty() {
                str_to_num(&doc.string_value(c.node))
            } else {
                ev(doc, &args[0], c)?.number(doc)
            })
        }
        "sum" => {
            arity(name, args, 1, 1)?;
            Value::Num(
                nodeset(0)?
                    .iter()
                    .map(|n| str_to_num(&doc.string_value(*n)))
                    .sum(),
            )
        }
        "floor" => {
            arity(name, args, 1, 1)?;
            Value::Num(num(0)?.floor())
        }
        "ceiling" => {
            arity(name, args, 1, 1)?;
            Value::Num(num(0)?.ceil())
        }
        "round" => {
            arity(name, args, 1, 1)?;
            Value::Num(round_half_up(num(0)?))
        }
        other => {
            return Err(format!(
                "`{other}()` is not an XPath 1.0 core function (the library is \
                 last, position, count, id, local-name, namespace-uri, name, string, \
                 concat, starts-with, contains, substring-before, substring-after, \
                 substring, string-length, normalize-space, translate, boolean, not, \
                 true, false, lang, number, sum, floor, ceiling, round)"
            ))
        }
    })
}

/// §4.4 `round()`: "the number that is closest to the argument and that is an
/// integer. If there are two such numbers, then the one that is closest to
/// positive infinity is returned." Rust's `f64::round` breaks ties AWAY FROM
/// ZERO instead, so `(-1.5).round()` is `-2` where XPath requires `-1`.
fn round_half_up(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    (x + 0.5).floor()
}

/// Render a value as the output lines arb's stream carries.
pub fn render(doc: &Doc, v: &Value) -> Vec<String> {
    match v {
        Value::NodeSet(ns) => ns.iter().map(|n| doc.render(*n)).collect(),
        Value::Str(s) => vec![s.clone()],
        Value::Num(n) => vec![fmt_number(*n)],
        Value::Bool(b) => vec![if *b { "true" } else { "false" }.to_string()],
    }
}
