//! XPath 1.0 lexer, AST and parser.
//!
//! Grammar and section numbers throughout are from the W3C Recommendation of
//! 16 November 1999, <https://www.w3.org/TR/1999/REC-xpath-19991116/>.
//!
//! This replaces a translator that compiled XPath-shaped syntax to a CSS
//! selector. That approach could not carry axes, functions, positional
//! predicates or anything else CSS has no spelling for — and worse, it made
//! `/li/text()` mean "descendant li", so a ROOTED path answered with a
//! non-empty node set where XPath selects nothing. A real parser plus a real
//! evaluator ([`crate::xpath_eval`]) removes that whole class of answer.

use std::fmt;

// ── AST ─────────────────────────────────────────────────────────────────────

/// The 13 axes of §2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    SelfAxis,
}

impl Axis {
    fn from_name(s: &str) -> Option<Axis> {
        Some(match s {
            "ancestor" => Axis::Ancestor,
            "ancestor-or-self" => Axis::AncestorOrSelf,
            "attribute" => Axis::Attribute,
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "descendant-or-self" => Axis::DescendantOrSelf,
            "following" => Axis::Following,
            "following-sibling" => Axis::FollowingSibling,
            "namespace" => Axis::Namespace,
            "parent" => Axis::Parent,
            "preceding" => Axis::Preceding,
            "preceding-sibling" => Axis::PrecedingSibling,
            "self" => Axis::SelfAxis,
            _ => return None,
        })
    }

    /// §2.2: "the ancestor, ancestor-or-self, preceding, and preceding-sibling
    /// axes are reverse axes; all other axes are forward axes."
    ///
    /// This decides the PROXIMITY POSITION a predicate sees (§2.4): on a reverse
    /// axis the first node is the one nearest the context node in REVERSE
    /// document order, so `preceding-sibling::li[1]` is the immediately
    /// preceding sibling, not the first one in the document.
    pub fn is_reverse(self) -> bool {
        matches!(
            self,
            Axis::Ancestor | Axis::AncestorOrSelf | Axis::Preceding | Axis::PrecedingSibling
        )
    }

    /// §2.3: "every axis has a principal node type … For the attribute axis, the
    /// principal node type is attribute. For the namespace axis, the principal
    /// node type is namespace. For other axes, the principal node type is
    /// element." This is what a name test or `*` selects.
    pub fn principal(self) -> Principal {
        match self {
            Axis::Attribute => Principal::Attribute,
            Axis::Namespace => Principal::Namespace,
            _ => Principal::Element,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    Element,
    Attribute,
    Namespace,
}

/// §2.3 NodeTest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTest {
    /// `*` — any node of the axis's principal type.
    Any,
    /// `prefix:*`.
    AnyInPrefix(String),
    /// A QName. HTML has no namespaces to speak of, so the whole QName is
    /// matched against the node's name, which is also what libxml2 does for
    /// HTML input.
    Name(String),
    /// `node()`.
    Node,
    /// `text()`.
    Text,
    /// `comment()`.
    Comment,
    /// `processing-instruction()` or `processing-instruction('target')`.
    Pi(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub axis: Axis,
    pub test: NodeTest,
    pub preds: Vec<Expr>,
}

/// Where a path's step list starts.
#[derive(Debug, Clone, PartialEq)]
pub enum PathStart {
    /// `/…` — the root node of the document (§2).
    Root,
    /// `a/b` — the context node.
    Relative,
    /// §3.3 PathExpr: a FilterExpr followed by `/` or `//` steps.
    Filter(Box<Expr>, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelOp {
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Rel(RelOp, Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Mod(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
    Path(PathStart, Vec<Step>),
    Number(f64),
    Literal(String),
    Call(String, Vec<Expr>),
}

// ── lexer (§3.7) ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Slash,
    DblSlash,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DblDot,
    At,
    Comma,
    DblColon,
    Star,
    Pipe,
    Plus,
    Minus,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    /// `and` / `or` / `mod` / `div` recognized as an operator by §3.7's rule.
    Op(&'static str),
    Num(f64),
    Str(String),
    /// An NCName or QName. `is_fn` records that a `(` follows (a NodeType or
    /// FunctionName), `is_axis` that a `::` follows.
    Name {
        text: String,
        is_fn: bool,
        is_axis: bool,
    },
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Slash => f.write_str("/"),
            Tok::DblSlash => f.write_str("//"),
            Tok::LParen => f.write_str("("),
            Tok::RParen => f.write_str(")"),
            Tok::LBracket => f.write_str("["),
            Tok::RBracket => f.write_str("]"),
            Tok::Dot => f.write_str("."),
            Tok::DblDot => f.write_str(".."),
            Tok::At => f.write_str("@"),
            Tok::Comma => f.write_str(","),
            Tok::DblColon => f.write_str("::"),
            Tok::Star => f.write_str("*"),
            Tok::Pipe => f.write_str("|"),
            Tok::Plus => f.write_str("+"),
            Tok::Minus => f.write_str("-"),
            Tok::Eq => f.write_str("="),
            Tok::Ne => f.write_str("!="),
            Tok::Lt => f.write_str("<"),
            Tok::Gt => f.write_str(">"),
            Tok::Le => f.write_str("<="),
            Tok::Ge => f.write_str(">="),
            Tok::Op(s) => f.write_str(s),
            Tok::Num(n) => write!(f, "{n}"),
            Tok::Str(s) => write!(f, "'{s}'"),
            Tok::Name { text, .. } => f.write_str(text),
        }
    }
}

/// Is `c` allowed to start an XML Name? XPath 1.0 defers to XML's `NameStartChar`
/// via NCName; HTML tag and attribute names are ASCII in practice, and `_`/`:`
/// are the only non-alphabetic starters worth carrying. Non-ASCII letters are
/// accepted wholesale rather than table-driven — refusing a valid name would be
/// a worse error than accepting an exotic one no HTML document contains.
fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

/// Tokenize an XPath expression.
///
/// §3.7's disambiguation rules are the whole reason this cannot be a naive
/// scanner, and both are implemented here:
///
/// * "If there is a preceding token and the preceding token is not one of `@`,
///   `::`, `(`, `[`, `,` or an Operator, then a `*` must be recognized as a
///   MultiplyOperator and an NCName must be recognized as an OperatorName." That
///   is what keeps `//div` (a name test) apart from `4 div 2` (an operator), and
///   `//*` (a name test) apart from `2 * 3`.
/// * "If the character following an NCName (possibly after intervening
///   ExprWhitespace) is `(`, then the token must be recognized as a NodeType or
///   a FunctionName" — and as an AxisName if followed by `::`.
pub fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out: Vec<Tok> = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // The §3.7 predicate: does the PREVIOUS token allow an operator here?
        let after_operand = matches!(
            out.last(),
            Some(
                Tok::RParen
                    | Tok::RBracket
                    | Tok::Num(_)
                    | Tok::Str(_)
                    | Tok::Star
                    | Tok::Dot
                    | Tok::DblDot
                    | Tok::Name { .. }
            )
        );
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            '@' => {
                out.push(Tok::At);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            '+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                // Both readings are the same token; the PARSER decides, using the
                // same "is an operator legal here" state. Recording it as `Star`
                // keeps the lexer from needing the grammar.
                out.push(Tok::Star);
                i += 1;
            }
            '=' => {
                out.push(Tok::Eq);
                i += 1;
            }
            '!' if cs.get(i + 1) == Some(&'=') => {
                out.push(Tok::Ne);
                i += 2;
            }
            '!' => return Err("`!` must be part of `!=`".into()),
            '<' if cs.get(i + 1) == Some(&'=') => {
                out.push(Tok::Le);
                i += 2;
            }
            '<' => {
                out.push(Tok::Lt);
                i += 1;
            }
            '>' if cs.get(i + 1) == Some(&'=') => {
                out.push(Tok::Ge);
                i += 2;
            }
            '>' => {
                out.push(Tok::Gt);
                i += 1;
            }
            '/' if cs.get(i + 1) == Some(&'/') => {
                out.push(Tok::DblSlash);
                i += 2;
            }
            '/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            ':' if cs.get(i + 1) == Some(&':') => {
                out.push(Tok::DblColon);
                i += 2;
            }
            '\'' | '"' => {
                // §3.7 Literal. Both quote characters are legal and neither
                // escapes: a literal delimited by `'` may contain `"` and vice
                // versa. An UNTERMINATED literal is an error, never a silently
                // truncated string.
                let quote = c;
                i += 1;
                let start = i;
                while i < cs.len() && cs[i] != quote {
                    i += 1;
                }
                if i >= cs.len() {
                    return Err(format!("unterminated string literal ({quote}…)"));
                }
                out.push(Tok::Str(cs[start..i].iter().collect()));
                i += 1;
            }
            '.' if cs.get(i + 1) == Some(&'.') => {
                out.push(Tok::DblDot);
                i += 2;
            }
            // §3.7 Number ::= Digits ('.' Digits?)? | '.' Digits. A `.` that
            // begins a number is only a number when a digit follows it.
            '.' if cs.get(i + 1).is_some_and(char::is_ascii_digit) => {
                let start = i;
                i += 1;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Tok::Num(num(&cs[start..i])?));
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    i += 1;
                }
                if cs.get(i) == Some(&'.') {
                    i += 1;
                    while i < cs.len() && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                out.push(Tok::Num(num(&cs[start..i])?));
            }
            c if is_name_start(c) => {
                let start = i;
                while i < cs.len() && is_name_char(cs[i]) {
                    i += 1;
                }
                // A QName's single `:` joins two NCNames. `::` is the axis
                // separator and must NOT be eaten here.
                if cs.get(i) == Some(&':') && cs.get(i + 1).is_some_and(|c| is_name_start(*c)) {
                    i += 1;
                    while i < cs.len() && is_name_char(cs[i]) {
                        i += 1;
                    }
                } else if cs.get(i) == Some(&':') && cs.get(i + 1) == Some(&'*') {
                    i += 2;
                }
                let text: String = cs[start..i].iter().collect();
                // Look ahead past whitespace for `(` and `::`.
                let mut j = i;
                while j < cs.len() && cs[j].is_whitespace() {
                    j += 1;
                }
                let is_fn = cs.get(j) == Some(&'(');
                let is_axis = cs.get(j) == Some(&':') && cs.get(j + 1) == Some(&':');
                // The OperatorName rule: only when an operator is legal here and
                // the name is not itself being used as a function or an axis.
                if after_operand && !is_fn && !is_axis {
                    match text.as_str() {
                        "and" | "or" | "mod" | "div" => {
                            out.push(Tok::Op(match text.as_str() {
                                "and" => "and",
                                "or" => "or",
                                "mod" => "mod",
                                _ => "div",
                            }));
                            continue;
                        }
                        _ => {}
                    }
                }
                out.push(Tok::Name {
                    text,
                    is_fn,
                    is_axis,
                });
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(out)
}

fn num(cs: &[char]) -> Result<f64, String> {
    let s: String = cs.iter().collect();
    s.parse::<f64>().map_err(|_| format!("bad number `{s}`"))
}

// ── parser (§3.1–§3.5, §2) ──────────────────────────────────────────────────

struct P {
    toks: Vec<Tok>,
    i: usize,
}

/// Parse a complete XPath 1.0 expression, or report why it is not one.
///
/// Trailing junk is an ERROR rather than being ignored: an expression this
/// parser only half-understands must never reach the evaluator, because a
/// half-understood path is exactly how the old translator produced answers that
/// looked right and were not.
pub fn parse(src: &str) -> Result<Expr, String> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err("empty expression".into());
    }
    let mut p = P { toks, i: 0 };
    let e = p.expr()?;
    if p.i != p.toks.len() {
        return Err(format!(
            "unexpected `{}` after a complete expression",
            p.toks[p.i]
        ));
    }
    Ok(e)
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(match self.peek() {
                Some(g) => format!("expected `{t}`, found `{g}`"),
                None => format!("expected `{t}`, found end of expression"),
            })
        }
    }

    // §3.4 OrExpr ::= AndExpr | OrExpr 'or' AndExpr
    fn expr(&mut self) -> Result<Expr, String> {
        let mut l = self.and_expr()?;
        while self.eat(&Tok::Op("or")) {
            l = Expr::Or(Box::new(l), Box::new(self.and_expr()?));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.eq_expr()?;
        while self.eat(&Tok::Op("and")) {
            l = Expr::And(Box::new(l), Box::new(self.eq_expr()?));
        }
        Ok(l)
    }

    fn eq_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.rel_expr()?;
        loop {
            if self.eat(&Tok::Eq) {
                l = Expr::Eq(Box::new(l), Box::new(self.rel_expr()?));
            } else if self.eat(&Tok::Ne) {
                l = Expr::Ne(Box::new(l), Box::new(self.rel_expr()?));
            } else {
                return Ok(l);
            }
        }
    }

    fn rel_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.add_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => RelOp::Lt,
                Some(Tok::Gt) => RelOp::Gt,
                Some(Tok::Le) => RelOp::Le,
                Some(Tok::Ge) => RelOp::Ge,
                _ => return Ok(l),
            };
            self.i += 1;
            l = Expr::Rel(op, Box::new(l), Box::new(self.add_expr()?));
        }
    }

    fn add_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.mul_expr()?;
        loop {
            if self.eat(&Tok::Plus) {
                l = Expr::Add(Box::new(l), Box::new(self.mul_expr()?));
            } else if self.eat(&Tok::Minus) {
                l = Expr::Sub(Box::new(l), Box::new(self.mul_expr()?));
            } else {
                return Ok(l);
            }
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.unary_expr()?;
        loop {
            // A `*` here is the MultiplyOperator: the parser is in a position
            // where an operand has just been read, which is §3.7's own rule.
            if self.eat(&Tok::Star) {
                l = Expr::Mul(Box::new(l), Box::new(self.unary_expr()?));
            } else if self.eat(&Tok::Op("div")) {
                l = Expr::Div(Box::new(l), Box::new(self.unary_expr()?));
            } else if self.eat(&Tok::Op("mod")) {
                l = Expr::Mod(Box::new(l), Box::new(self.unary_expr()?));
            } else {
                return Ok(l);
            }
        }
    }

    // §3.5 UnaryExpr ::= UnionExpr | '-' UnaryExpr
    fn unary_expr(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Neg(Box::new(self.unary_expr()?)));
        }
        self.union_expr()
    }

    fn union_expr(&mut self) -> Result<Expr, String> {
        let mut l = self.path_expr()?;
        while self.eat(&Tok::Pipe) {
            l = Expr::Union(Box::new(l), Box::new(self.path_expr()?));
        }
        Ok(l)
    }

    /// §3.3 PathExpr ::= LocationPath
    ///                 | FilterExpr
    ///                 | FilterExpr '/' RelativeLocationPath
    ///                 | FilterExpr '//' RelativeLocationPath
    fn path_expr(&mut self) -> Result<Expr, String> {
        // A PrimaryExpr start means this is a FilterExpr, not a LocationPath.
        // §2.3's four NodeTypes are spelled like function calls and are not
        // ones: `text()` starting an expression is a location STEP
        // (`child::text()`), which is what `//a[text()="X"]` means. Reading it as
        // a FunctionCall made that predicate a hard error on a path xmllint
        // answers.
        let node_type_ahead = matches!(
            self.peek(),
            Some(Tok::Name { text, is_fn: true, .. })
                if matches!(text.as_str(), "node" | "text" | "comment" | "processing-instruction")
        );
        let is_filter = !node_type_ahead
            && (matches!(
                self.peek(),
                Some(Tok::Num(_)) | Some(Tok::Str(_)) | Some(Tok::LParen)
            ) || matches!(self.peek(), Some(Tok::Name { is_fn: true, .. })));
        if is_filter {
            let base = self.primary_expr()?;
            let mut preds = Vec::new();
            while self.eat(&Tok::LBracket) {
                preds.push(self.expr()?);
                self.expect(&Tok::RBracket)?;
            }
            let mut steps = Vec::new();
            if self.eat(&Tok::DblSlash) {
                steps.push(descendant_or_self());
                steps.extend(self.relative_path()?);
            } else if self.eat(&Tok::Slash) {
                steps.extend(self.relative_path()?);
            }
            // §3.3: a bare FilterExpr IS a PathExpr, and its value is the
            // primary expression's own — `'/x'` is the string `/x` and
            // `last()` is a number. Wrapping those in a Path would demand a
            // node-set from a value that is not one, which is how
            // `//a[@href='/x']` came back as an error instead of a match.
            if preds.is_empty() && steps.is_empty() {
                return Ok(base);
            }
            return Ok(Expr::Path(PathStart::Filter(Box::new(base), preds), steps));
        }
        self.location_path()
    }

    // §2 LocationPath ::= RelativeLocationPath | AbsoluteLocationPath
    fn location_path(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::DblSlash) {
            // §2.5: `//x` abbreviates `/descendant-or-self::node()/x`.
            let mut steps = vec![descendant_or_self()];
            steps.extend(self.relative_path()?);
            return Ok(Expr::Path(PathStart::Root, steps));
        }
        if self.eat(&Tok::Slash) {
            // A lone `/` is the root node itself. `/` followed by a step is a
            // path FROM the root — which is what makes `/li` select nothing on a
            // document whose root child is `html`, the bug this engine exists to
            // fix.
            if self.starts_step() {
                let steps = self.relative_path()?;
                return Ok(Expr::Path(PathStart::Root, steps));
            }
            return Ok(Expr::Path(PathStart::Root, Vec::new()));
        }
        let steps = self.relative_path()?;
        Ok(Expr::Path(PathStart::Relative, steps))
    }

    /// Can the current token begin a Step?
    fn starts_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(Tok::At | Tok::Dot | Tok::DblDot | Tok::Star | Tok::Name { .. })
        )
    }

    fn relative_path(&mut self) -> Result<Vec<Step>, String> {
        let mut steps = vec![self.step()?];
        loop {
            if self.eat(&Tok::DblSlash) {
                steps.push(descendant_or_self());
                steps.push(self.step()?);
            } else if self.eat(&Tok::Slash) {
                steps.push(self.step()?);
            } else {
                return Ok(steps);
            }
        }
    }

    // §2.1 Step ::= AxisSpecifier NodeTest Predicate* | AbbreviatedStep
    fn step(&mut self) -> Result<Step, String> {
        // §2.5 AbbreviatedStep: `.` is `self::node()`, `..` is `parent::node()`.
        if self.eat(&Tok::Dot) {
            return Ok(Step {
                axis: Axis::SelfAxis,
                test: NodeTest::Node,
                preds: Vec::new(),
            });
        }
        if self.eat(&Tok::DblDot) {
            return Ok(Step {
                axis: Axis::Parent,
                test: NodeTest::Node,
                preds: Vec::new(),
            });
        }
        // §2.5: `@` abbreviates `attribute::`.
        let axis = if self.eat(&Tok::At) {
            Axis::Attribute
        } else if let Some(Tok::Name {
            text,
            is_axis: true,
            ..
        }) = self.peek()
        {
            let name = text.clone();
            let a =
                Axis::from_name(&name).ok_or_else(|| format!("`{name}` is not an XPath axis"))?;
            self.i += 1;
            self.expect(&Tok::DblColon)?;
            a
        } else {
            Axis::Child
        };
        let test = self.node_test()?;
        let mut preds = Vec::new();
        while self.eat(&Tok::LBracket) {
            preds.push(self.expr()?);
            self.expect(&Tok::RBracket)?;
        }
        Ok(Step { axis, test, preds })
    }

    // §2.3 NodeTest ::= NameTest | NodeType '(' ')' | 'processing-instruction' '(' Literal ')'
    fn node_test(&mut self) -> Result<NodeTest, String> {
        if self.eat(&Tok::Star) {
            return Ok(NodeTest::Any);
        }
        let (text, is_fn) = match self.peek() {
            Some(Tok::Name { text, is_fn, .. }) => (text.clone(), *is_fn),
            Some(t) => return Err(format!("expected a node test, found `{t}`")),
            None => return Err("expected a node test, found end of expression".into()),
        };
        self.i += 1;
        if is_fn {
            self.expect(&Tok::LParen)?;
            let t = match text.as_str() {
                "node" => NodeTest::Node,
                "text" => NodeTest::Text,
                "comment" => NodeTest::Comment,
                "processing-instruction" => {
                    if let Some(Tok::Str(s)) = self.peek() {
                        let s = s.clone();
                        self.i += 1;
                        NodeTest::Pi(Some(s))
                    } else {
                        NodeTest::Pi(None)
                    }
                }
                other => {
                    return Err(format!(
                        "`{other}()` is not a node type (expected node, text, comment or \
                         processing-instruction)"
                    ))
                }
            };
            self.expect(&Tok::RParen)?;
            return Ok(t);
        }
        if let Some(prefix) = text.strip_suffix(":*") {
            return Ok(NodeTest::AnyInPrefix(prefix.to_string()));
        }
        Ok(NodeTest::Name(text))
    }

    // §3.1 PrimaryExpr ::= VariableReference | '(' Expr ')' | Literal | Number | FunctionCall
    fn primary_expr(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                Ok(Expr::Number(n))
            }
            Some(Tok::Str(s)) => {
                self.i += 1;
                Ok(Expr::Literal(s))
            }
            Some(Tok::LParen) => {
                self.i += 1;
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Name {
                text, is_fn: true, ..
            }) => {
                self.i += 1;
                self.expect(&Tok::LParen)?;
                let mut args = Vec::new();
                if !self.eat(&Tok::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RParen)?;
                        break;
                    }
                }
                Ok(Expr::Call(text, args))
            }
            Some(t) => Err(format!("expected an expression, found `{t}`")),
            None => Err("expected an expression, found end of expression".into()),
        }
    }
}

/// The step `//` abbreviates (§2.5): `descendant-or-self::node()`.
fn descendant_or_self() -> Step {
    Step {
        axis: Axis::DescendantOrSelf,
        test: NodeTest::Node,
        preds: Vec::new(),
    }
}
