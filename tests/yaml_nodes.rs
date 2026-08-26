//! The node-preserving YAML value model: what it must carry, and what it must
//! not disturb.
//!
//! `scripts/jq_parity.sh` scores this leg against the reference binaries. These
//! tests cover the two properties a differential harness cannot see, because
//! both are about arb disagreeing with ITSELF:
//!
//! * **The box is invisible to jq.** `JqVal::Node` is a seventh variant of the
//!   value model, and every operation whose answer is about the VALUE has to
//!   unwrap it first. A missed unwrap is a silent wrong answer on YAML input and
//!   is invisible against yq, which would have to be asked the same wrong
//!   question to notice. `metadata_never_changes_a_jq_answer` runs a corpus of
//!   filters over the same document read WITH metadata and with every box
//!   stripped, and requires the two to agree — so the check is arb against arb,
//!   and it fails on the first arm anyone forgets.
//! * **The reader and the writer are inverses.** `yq '.' file` gives the file
//!   back, and so must `in.yaml; out.yaml`.

use arb::jqlang::{self, JqVal};
use arb::ynode::{self, Emit, NodeMeta};
use std::rc::Rc;

/// The same value with every `JqVal::Node` removed, recursively — what the
/// document would have been if it had been read from JSON.
fn strip(v: &JqVal) -> JqVal {
    match v.bare() {
        JqVal::Arr(a) => JqVal::arr(a.iter().map(strip).collect()),
        JqVal::Obj(m) => JqVal::obj(m.iter().map(|(k, val)| (k.clone(), strip(val))).collect()),
        other => other.clone(),
    }
}

fn run(prog: &jqlang::Program, v: &JqVal) -> Result<Vec<String>, String> {
    let interp = jqlang::Interp::default();
    interp.set_doc(v);
    prog.run(&interp, v)
        .map(|out| out.iter().map(jqlang::render).collect())
        .map_err(|e| e.to_message())
}

/// A document carrying every kind of metadata the model records, so a filter
/// that meets a box meets a real one.
const DOC: &str = "\
# doc head
name: widget # trailing
base: &anc
  k: v
  n: 7
use: *anc
merged:
  <<: *anc
  extra: 1
quoted: \"dq\"
single: 'sq'
lit: |
  line1
  line2
flow: {a: 1, b: [2, 3]}
tagged: !!str 123
empty:
hexed: 0x10
ratio: 1.50
nums: [3, 1, 2]
items:
  - id: 2
    v: 10
  - id: 1
    v: 5
";

/// Every filter here is a JQ question — its answer is about values, and the
/// YAML metadata must not reach it. Chosen to walk the arms that inspect a
/// value's SHAPE, which are the ones a missed unwrap breaks: comparison,
/// arithmetic, sorting, grouping, indexing, iteration, path assignment,
/// destructuring, string interpolation and rendering.
const FILTERS: &[&str] = &[
    ".",
    ".name",
    ".base",
    ".base.k",
    ".use",
    ".merged",
    ".flow.b[1]",
    ".nums | sort",
    ".nums | sort_by(-.)",
    ".nums | group_by(. % 2)",
    ".nums | unique",
    ".nums | min, max",
    ".nums | add",
    ".nums | map(. * 2)",
    ".nums | reduce .[] as $x (0; . + $x)",
    ".items | sort_by(.id)",
    ".items | map(.v) | add",
    ".items[] | select(.v > 6)",
    ".ratio + 0",
    ".ratio == 1.5",
    ".hexed + 1",
    ".name == \"widget\"",
    ".name | ascii_upcase",
    ".name | explode | implode",
    ".name | tostring",
    ".name | tojson",
    ".lit | split(\"\\n\")",
    ".quoted | test(\"d\")",
    ".quoted | sub(\"d\"; \"D\")",
    "keys",
    "keys_unsorted",
    "to_entries",
    "with_entries(.value |= 1)",
    "[paths]",
    "[leaf_paths]",
    "getpath([\"base\",\"k\"])",
    "setpath([\"name\"]; \"z\")",
    "delpaths([[\"name\"]])",
    "del(.name)",
    ".name = \"z\"",
    ".base.n |= . + 1",
    ".nums[0] = 9",
    "[.. | numbers]",
    "[..] | length",
    ".empty == null",
    ".empty // \"fallback\"",
    ". as {name: $n} | $n",
    ".items[0] as {$id, $v} | [$id, $v]",
    "\"n=\\(.name) r=\\(.ratio)\"",
    "@json \"\\(.base)\"",
    ".flow | type",
    ".tagged | type",
    ".base | length",
    "has(\"name\")",
    "contains({name: \"widget\"})",
    "[.nums[] | tostring] | join(\",\")",
    "any(.nums[]; . > 2)",
    "all(.nums[]; . > 0)",
    "[limit(2; .nums[])]",
    "[first(.nums[]), last(.nums[])]",
    "flatten",
    "[.items[] | to_entries[] | .key]",
    ".nums | index(1)",
    ".nums[1:]",
    ".lit[0:4]",
    "walk(if type == \"number\" then . + 1 else . end)",
    "[recurse | strings] | length",
    "tostream | length",
    "map_values(1)",
    "if .ratio > 1 then \"big\" else \"small\" end",
    "try (.name | tonumber) catch \"nope\"",
    ".name | @base64 | @base64d",
    // Added as the surface grew. Each is a jq question whose answer must not
    // change when the input carries metadata.
    ".nums | pick([0])",
    "pick(.name, .ratio)",
    ".base | to_entries | from_entries",
    ".items | map(.id) | sort",
    ".nums as $n | $n | add",
    ".items[] as {$id} | $id",
    "[paths(type == \"number\")]",
    "path(.base.k)",
    "[.items[] | .v] | sort | reverse",
    ".flow | to_entries | map(.key)",
    ".base * {n: 8}",
    ".nums - [1]",
    ".items | max_by(.v) | .id",
    ".items | min_by(.v) | .id",
    "[.[] | if type == \"array\" then length else empty end]",
    "reduce (.nums[]) as $x ({}; .[$x | tostring] = $x) | keys",
    ".lit | ltrimstr(\"line1\")",
    ".name | splits(\"-\") ",
    "[.. | select(type == \"object\") | keys_unsorted] | length",
    "{a: .name, b: .ratio} | tojson",
    ".items | INDEX(.id) | keys",
    ".nums | IN(1)",
    ".base | with_entries(select(.key == \"k\"))",
    "getpath([\"items\", 0, \"v\"])",
    ".items | tostream | length",
];

#[test]
fn metadata_never_changes_a_jq_answer() {
    let docs = arb::yaml::documents(DOC);
    let boxed = docs.first().expect("one document");
    let plain = strip(boxed);

    // The corpus is only meaningful if the metadata is actually there.
    assert!(
        boxed.meta().is_some(),
        "the composed document carries no metadata — the corpus would prove nothing"
    );

    for f in FILTERS {
        let prog = jqlang::Program::compile(f).unwrap_or_else(|e| panic!("compile `{f}`: {e}"));
        let with = run(&prog, boxed);
        let without = run(&prog, &plain);
        assert_eq!(
            with, without,
            "`{f}` answers differently with YAML node metadata than without it — \
             an operation is reading the box instead of the value"
        );
    }
}

/// The jq value model's own invariants, asserted directly on a boxed node: a
/// comment must not make a value compare, sort, or render differently.
#[test]
fn a_boxed_value_equals_its_bare_twin() {
    let meta = NodeMeta {
        line: Rc::from("a comment"),
        anchor: Rc::from("anc"),
        ..NodeMeta::default()
    };
    let boxed = JqVal::wrap(JqVal::num(1.0), meta);
    let bare = JqVal::num(1.0);

    assert!(
        matches!(boxed, JqVal::Node(_)),
        "the box was optimised away"
    );
    assert!(jqlang::eq_vals(&boxed, &bare), "a comment changed equality");
    assert_eq!(jqlang::render(&boxed), jqlang::render(&bare));
    assert_eq!(boxed.type_name(), "number");
    assert_eq!(boxed.truthy(), bare.truthy());
    assert_eq!(
        jqlang::cmp_vals(&boxed, &bare),
        std::cmp::Ordering::Equal,
        "a comment changed the sort order"
    );
}

/// Metadata that records NOTHING is dropped rather than boxed, so a JSON value
/// that passes through a metadata assignment comes back bit-identical.
#[test]
fn empty_metadata_is_not_boxed() {
    let v = JqVal::wrap(JqVal::str("x"), NodeMeta::default());
    assert!(
        !matches!(v, JqVal::Node(_)),
        "a node with nothing to record was boxed anyway"
    );
}

fn roundtrip(src: &str) -> String {
    let docs = arb::yaml::documents(src);
    ynode::emit_docs(&docs, Emit::default())
}

/// One case per feature the model has to carry. The parity harness compares
/// these against the reference; here they are pinned so a change that breaks one
/// fails in `cargo test` too, on a machine with no `yq` installed.
#[test]
fn a_document_survives_the_round_trip() {
    for (label, src) in [
        (
            "comments in every position",
            "# doc head\na: 1 # a line\n# b head\nb: 2\nc:\n  # d head\n  d: 4 # d line\n  # e foot\n\n  e: 5\n# tail\n",
        ),
        (
            "anchors, aliases and merge keys",
            "defaults: &def\n  retries: 3\nprod:\n  <<: *def\n  timeout: 60\ndev: *def\n",
        ),
        (
            "all six scalar styles",
            "plain: hello\nsingle: 'it''s here'\ndouble: \"a\\ttab\"\nliteral: |\n  keep\n  the breaks\nfolded: >-\n  fold these\n  two lines\nempty:\n",
        ),
        (
            "flow versus block",
            "flowmap: {a: 1, b: 2}\nflowseq: [1, 2, 3]\nblockmap:\n  a: 1\nemptymap: {}\nemptyseq: []\n",
        ),
        ("tags", "s: !!str 123\ni: !!int 7\ncustom: !mytag payload\n"),
        (
            "empty values and nulls",
            "blank:\nexplicit: null\ntilde: ~\nemptystr: \"\"\nfalse: false\n",
        ),
        (
            "non-ASCII",
            "greek: αβγ\narrows: \"a → b\"\nemoji: 🚀\nkey→: value\n",
        ),
        (
            "number spellings",
            "padded: 007\nhex: 0x1F\nfloat: 1.50\nexp: 1e3\nneg: -42\n",
        ),
        (
            "multi-document stream",
            "# first\na: 1\n---\n# second\nb: 2\n",
        ),
        (
            "deeply nested anchors",
            "outer: &o\n  inner: &i\n    deep: &d\n      leaf: 1\n    other: *d\n  second: *i\ntop: *o\n",
        ),
        (
            "aliases inside merge keys",
            "base: &b\n  a: 1\nextra: &e\n  b: 2\nboth:\n  <<: [*b, *e]\n  c: 3\n",
        ),
        (
            "a comment on the sequence marker line",
            "nested:\n  - # head inside item\n    k: v # line of k\n",
        ),
        (
            "a foot comment on a sequence's last item",
            "seq:\n  - one\n  - two\n  # foot of the seq\n\nmap:\n  a: 1\n",
        ),
        (
            "an explicit leading document marker",
            "---\na: 1\n---\nb: 2\n",
        ),
    ] {
        assert_eq!(roundtrip(src), src, "round trip lost something: {label}");
    }
}

/// The metadata the accessors read, pinned at the values `yq v4.53.6` reports
/// for the same nodes.
#[test]
fn the_reader_records_what_yq_reports() {
    let docs = arb::yaml::documents(DOC);
    let doc = &docs[0];

    let at = |path: &[&str]| -> JqVal {
        let mut cur = doc.clone();
        for k in path {
            cur = match cur.bare() {
                JqVal::Obj(m) => m
                    .iter()
                    .find(|(key, _)| &**key == *k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(JqVal::Null),
                _ => JqVal::Null,
            };
        }
        cur
    };

    let m = doc.meta().expect("document metadata");
    assert_eq!(&*m.head, "doc head");

    let name = at(&["name"]);
    let nm = name.meta().expect("`.name` metadata");
    assert_eq!(&*nm.line, "trailing");
    // 1-based, counted from the stream's first CONTENT line — the leading
    // comment block is not counted, which is what `yq '.name | line'` reports.
    assert_eq!(nm.line_no, 1);
    assert_eq!(nm.col_no, 7);
    assert_eq!(
        nm.key.as_deref().map(jqlang::render_raw).as_deref(),
        Some("name")
    );

    // An anchored collection is positioned at its `&anc`, a line above the
    // mapping's first key.
    let base = at(&["base"]);
    let bm = base.meta().expect("`.base` metadata");
    assert_eq!(&*bm.anchor, "anc");
    assert_eq!(bm.line_no, 2);
    assert_eq!(bm.col_no, 7);
    assert_eq!(ynode::kind_of(&base), "map");

    // An alias keeps its OWN box: it re-emits as `*anc` rather than inheriting
    // the anchored node's `&anc` and declaring it a second time.
    let use_ = at(&["use"]);
    assert_eq!(&*use_.meta().expect("`.use` metadata").alias, "anc");
    assert_eq!(ynode::kind_of(&use_), "alias");

    // A merge key is resolved for READERS and kept for the WRITER.
    let merged = at(&["merged"]);
    assert_eq!(jqlang::render(&merged), r#"{"k":"v","n":7,"extra":1}"#);
    assert!(
        merged.meta().is_some_and(|m| m.written.is_some()),
        "the pre-merge entry list was not kept, so `<<` cannot round-trip"
    );

    assert_eq!(
        at(&["quoted"]).meta().map(|m| m.style.name()),
        Some("double")
    );
    assert_eq!(
        at(&["single"]).meta().map(|m| m.style.name()),
        Some("single")
    );
    assert_eq!(at(&["lit"]).meta().map(|m| m.style.name()), Some("literal"));
    assert_eq!(at(&["flow"]).meta().map(|m| m.style.name()), Some("flow"));
    assert_eq!(at(&["name"]).meta().map(|m| m.style.name()), Some(""));

    assert_eq!(&*at(&["tagged"]).meta().expect("tag").tag, "!!str");
    assert_eq!(ynode::implicit_tag(&at(&["ratio"])), "!!float");
    assert_eq!(ynode::implicit_tag(&at(&["base", "n"])), "!!int");
    assert_eq!(ynode::implicit_tag(&at(&["empty"])), "!!null");

    // A scalar whose rendering would not reproduce its source keeps the source.
    assert_eq!(&*at(&["hexed"]).meta().expect("hex").raw, "0x10");
    assert_eq!(jqlang::render(&at(&["hexed"])), "16");
    // A float already carries its literal, so it needs no source text.
    assert_eq!(jqlang::render(&at(&["ratio"])), "1.50");
}

/// A `- # note` on the marker line is the first KEY's head comment, which is
/// where `yq` files it: `.nested[0].k | key | head_comment` answers it, and
/// `.nested[0] | head_comment` is empty because the comment belongs one level in.
/// Pinned because the obvious reading — that it is the ITEM's head — round-trips
/// just as well and answers the accessors wrongly.
#[test]
fn a_marker_comment_belongs_to_the_first_key() {
    let docs = arb::yaml::documents("nested:\n  - # head inside item\n    k: v\n");
    let JqVal::Obj(root) = docs[0].bare() else {
        panic!("expected a mapping")
    };
    let JqVal::Arr(seq) = root[0].1.bare() else {
        panic!("expected a sequence")
    };
    let item = &seq[0];
    assert!(
        item.meta().is_some_and(|m| m.marker),
        "the item does not record that its first key's head was on the marker line"
    );
    assert_eq!(
        item.meta().map_or("", |m| &m.head),
        "",
        "the comment was filed on the item, where yq reports nothing"
    );
    let JqVal::Obj(inner) = item.bare() else {
        panic!("expected a mapping item")
    };
    let key_head = inner[0]
        .1
        .meta()
        .and_then(|m| m.key.clone())
        .and_then(|k| k.meta().map(|km| km.head.to_string()));
    assert_eq!(key_head.as_deref(), Some("head inside item"));
}

/// yq's OWN spellings, which the grammar accepts in the positions jq leaves
/// empty. Pinned here as well as in the parity harness so the extension cannot be
/// lost on a machine with no `yq` installed.
#[test]
fn yq_native_spellings_parse_and_answer() {
    let docs = arb::yaml::documents("a: 1\nb: two\n");
    let doc = &docs[0];
    let run1 = |src: &str| -> String {
        let prog = jqlang::Program::compile(src).unwrap_or_else(|e| panic!("compile `{src}`: {e}"));
        let interp = jqlang::Interp::default();
        interp.set_doc(doc);
        let out = prog
            .run(&interp, doc)
            .unwrap_or_else(|e| panic!("run `{src}`: {}", e.to_message()));
        ynode::emit_docs(&out, Emit::default())
    };
    // The metadata postfix, in yq's spelling and in arb's, must agree.
    assert_eq!(run1(r#".a anchor = "x""#), "a: &x 1\nb: two\n");
    assert_eq!(run1(r#".a |= (anchor = "x")"#), "a: &x 1\nb: two\n");
    assert_eq!(run1(r#".b style = "double""#), "a: 1\nb: \"two\"\n");
    assert_eq!(run1(r#".a line_comment = "hi""#), "a: 1 # hi\nb: two\n");
    // The postfix reduce, against jq's prefix form on the same stream.
    let sum = |src: &str| -> Vec<String> {
        let prog = jqlang::Program::compile(src).unwrap();
        let interp = jqlang::Interp::default();
        prog.run(&interp, &JqVal::Null)
            .unwrap()
            .iter()
            .map(jqlang::render)
            .collect()
    };
    assert_eq!(sum("[1,2,3] | .[] as $item ireduce (0; . + $item)"), ["6"]);
    assert_eq!(
        sum("[1,2,3] | .[] as $item ireduce (0; . + $item)"),
        sum("reduce [1,2,3][] as $item (0; . + $item)"),
    );
}

/// Key order is the DOCUMENT's, not sorted. `serde_json::Map` is a `BTreeMap`
/// and gave `.base` back alphabetised before the jq value model carried the
/// order end to end.
#[test]
fn key_order_is_file_order() {
    let docs = arb::yaml::documents("z: 1\nm: 2\na: 3\n");
    assert_eq!(jqlang::render(&docs[0]), r#"{"z":1,"m":2,"a":3}"#);
}
