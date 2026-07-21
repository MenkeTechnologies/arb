//! Arb's computational substrate: arithmetic expressions are compiled to a
//! `fusevm::Chunk` and executed on the fusevm VM (Cranelift JIT for hot chunks).
//! This is what "arb runs on fusevm" means concretely — every computed value in
//! a spec flows through fusevm bytecode, not a bespoke evaluator.
//!
//! `x` is the pipeline input scalar; a bareword identifier (`amount`, `latency`)
//! is a field of the current record, resolved by the caller's closure. Both are
//! baked into the chunk as constants per evaluation, so no VM slot state is
//! assumed across `run()`.
//! Comparisons (`== != < <= > >=`) lower to fusevm's `Num*` ops and yield a
//! boolean; `eval_pred` reads the result via `Value::is_truthy` for `where`.

use fusevm::vm::{VMResult, VM};
use fusevm::{ChunkBuilder, Op, Value};

#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Logical `and`/`or` over truthiness (each operand normalized to 0/1).
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    /// The pipeline input scalar.
    Var,
    /// A named field of the current record, resolved at eval time.
    Field(String),
    /// A live control reference (`.th` — an `input`/control widget's value),
    /// written `.name` in a predicate. `resolve_pipeline` substitutes it with a
    /// `Num` before eval when the control holds a number; unresolved -> NaN.
    Control(String),
    /// A string literal — only produced by substituting a string control.
    Str(String),
    /// `match(.q)` — substring test of the whole line against a control's text.
    /// Evaluated Rust-side (not on the numeric VM); empty control -> matches all.
    Match(Box<Expr>),
    /// `<field> in .lv` — set membership: keep lines whose FIELD value is in the
    /// control's comma-separated selected set. Empty set -> matches all.
    InSet(String, Box<Expr>),
    Neg(Box<Expr>),
    /// Logical negation: truthy input -> 0, falsy -> 1.
    Not(Box<Expr>),
    /// `name(arg, …)` — a call to an inline-Rust FFI export registered by a
    /// `rust { ... }` block. Numeric-only: each arg is evaluated to an f64 and
    /// marshaled as a `fusevm::Value::float`; the export's return is read back as
    /// an f64. An unregistered name (or a failed call) evaluates to NaN, so a
    /// comparison against it is simply false.
    Call(String, Vec<Expr>),
    /// Membership: `left in [a, b, c]` — truthy iff `left` equals any element
    /// (jq `IN`). Empty list is always falsy.
    InList(Box<Expr>, Vec<Expr>),
    /// Range membership: `left in lo..hi` — truthy iff `lo <= left <= hi`.
    InRange(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Ternary `cond ? then : else` — only the taken branch is evaluated (real
    /// fusevm branching), so a guarded `x != 0 ? 100/x : 0` never divides by 0.
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
}

/// Parse an expression: numbers, `x`, `+ - * / %`, comparisons
/// (`== != < <= > >=`), unary `-`, and parentheses.
pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = Parser {
        c: src.chars().collect(),
        i: 0,
        depth: 0,
    };
    let e = p.ternary()?;
    if p.peek().is_some() {
        return Err(format!("calc: unexpected `{}`", p.c[p.i]));
    }
    Ok(e)
}

/// Evaluate as a number with no field resolver (field refs -> NaN).
pub fn eval(e: &Expr, x: f64) -> Result<f64, String> {
    eval_ctx(e, x, &|_| f64::NAN)
}

/// Collect the names of every `Control` reference in `e` (e.g. `.th` -> "th").
pub fn control_names(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Control(n) => out.push(n.clone()),
        Expr::Num(_) | Expr::Var | Expr::Field(_) | Expr::Str(_) => {}
        Expr::Neg(a) | Expr::Not(a) | Expr::Match(a) | Expr::InSet(_, a) => control_names(a, out),
        Expr::Call(_, items) => items.iter().for_each(|it| control_names(it, out)),
        Expr::InList(l, items) => {
            control_names(l, out);
            items.iter().for_each(|it| control_names(it, out));
        }
        Expr::InRange(l, lo, hi) => {
            control_names(l, out);
            control_names(lo, out);
            control_names(hi, out);
        }
        Expr::Cond(a, b, c) => {
            control_names(a, out);
            control_names(b, out);
            control_names(c, out);
        }
        Expr::Bin(_, a, b) => {
            control_names(a, out);
            control_names(b, out);
        }
    }
}

/// Split control refs by the type they resolve to: `Match`/`InSet` inners are
/// string controls, everything else is numeric. Lets `resolve_pipeline` require
/// numeric controls to resolve (or drop the filter) while string controls with
/// an empty value simply match everything.
pub fn control_names_typed(e: &Expr, num: &mut Vec<String>, strv: &mut Vec<String>) {
    match e {
        Expr::Control(n) => num.push(n.clone()),
        Expr::Match(inner) | Expr::InSet(_, inner) => {
            if let Expr::Control(n) = inner.as_ref() {
                strv.push(n.clone());
            } else {
                control_names_typed(inner, num, strv);
            }
        }
        Expr::Num(_) | Expr::Var | Expr::Field(_) | Expr::Str(_) => {}
        Expr::Neg(a) | Expr::Not(a) => control_names_typed(a, num, strv),
        Expr::Call(_, items) => items
            .iter()
            .for_each(|it| control_names_typed(it, num, strv)),
        Expr::InList(l, items) => {
            control_names_typed(l, num, strv);
            items
                .iter()
                .for_each(|it| control_names_typed(it, num, strv));
        }
        Expr::InRange(l, lo, hi) => {
            control_names_typed(l, num, strv);
            control_names_typed(lo, num, strv);
            control_names_typed(hi, num, strv);
        }
        Expr::Cond(a, b, c) => {
            control_names_typed(a, num, strv);
            control_names_typed(b, num, strv);
            control_names_typed(c, num, strv);
        }
        Expr::Bin(_, a, b) => {
            control_names_typed(a, num, strv);
            control_names_typed(b, num, strv);
        }
    }
}

/// Whether `e` contains a string predicate (`match`/`in .set`) — the query layer
/// routes such a `where` to the Rust evaluator instead of the numeric VM.
pub fn expr_has_str(e: &Expr) -> bool {
    match e {
        Expr::Match(_) | Expr::InSet(..) | Expr::Str(_) => true,
        Expr::Num(_) | Expr::Var | Expr::Field(_) | Expr::Control(_) => false,
        Expr::Neg(a) | Expr::Not(a) => expr_has_str(a),
        // A Call yields an f64; numeric-only args carry no string predicate, but
        // recurse for consistency with every other compound node.
        Expr::Call(_, items) => items.iter().any(expr_has_str),
        Expr::InList(l, items) => expr_has_str(l) || items.iter().any(expr_has_str),
        Expr::InRange(l, lo, hi) => expr_has_str(l) || expr_has_str(lo) || expr_has_str(hi),
        Expr::Cond(a, b, c) => expr_has_str(a) || expr_has_str(b) || expr_has_str(c),
        Expr::Bin(_, a, b) => expr_has_str(a) || expr_has_str(b),
    }
}

/// Rewrite every `Control(name)` in `e` to `Control("{ns}.{name}")`, in place —
/// used by `import X as Y` to namespace a module's control references.
pub fn prefix_controls(e: &mut Expr, ns: &str) {
    match e {
        Expr::Control(n) => *n = format!("{ns}.{n}"),
        Expr::Num(_) | Expr::Var | Expr::Field(_) | Expr::Str(_) => {}
        Expr::Neg(a) | Expr::Not(a) | Expr::Match(a) | Expr::InSet(_, a) => prefix_controls(a, ns),
        Expr::Call(_, items) => items.iter_mut().for_each(|it| prefix_controls(it, ns)),
        Expr::InList(l, items) => {
            prefix_controls(l, ns);
            items.iter_mut().for_each(|it| prefix_controls(it, ns));
        }
        Expr::InRange(l, lo, hi) => {
            prefix_controls(l, ns);
            prefix_controls(lo, ns);
            prefix_controls(hi, ns);
        }
        Expr::Cond(a, b, c) => {
            prefix_controls(a, ns);
            prefix_controls(b, ns);
            prefix_controls(c, ns);
        }
        Expr::Bin(_, a, b) => {
            prefix_controls(a, ns);
            prefix_controls(b, ns);
        }
    }
}

/// Return a copy of `e` with control refs substituted: a numeric `Control` ->
/// `Num(num(n))` when resolvable (else left as-is -> NaN); a string control
/// inside `match`/`in .set` -> `Str(strv(n))` (empty when unset -> matches all).
pub fn substitute_controls(
    e: &Expr,
    num: &dyn Fn(&str) -> Option<f64>,
    strv: &dyn Fn(&str) -> Option<String>,
) -> Expr {
    let sub = |b: &Expr| Box::new(substitute_controls(b, num, strv));
    // Resolve a control ref to a string node (for match/in-set inners).
    let sub_str = |b: &Expr| -> Box<Expr> {
        match b {
            Expr::Control(n) => Box::new(Expr::Str(strv(n).unwrap_or_default())),
            other => Box::new(substitute_controls(other, num, strv)),
        }
    };
    match e {
        Expr::Control(n) => match num(n) {
            Some(v) => Expr::Num(v),
            None => Expr::Control(n.clone()),
        },
        Expr::Match(inner) => Expr::Match(sub_str(inner)),
        Expr::InSet(field, inner) => Expr::InSet(field.clone(), sub_str(inner)),
        Expr::Num(_) | Expr::Var | Expr::Field(_) | Expr::Str(_) => e.clone(),
        Expr::Call(name, items) => Expr::Call(
            name.clone(),
            items
                .iter()
                .map(|it| substitute_controls(it, num, strv))
                .collect(),
        ),
        Expr::Neg(a) => Expr::Neg(sub(a)),
        Expr::Not(a) => Expr::Not(sub(a)),
        Expr::InList(l, items) => Expr::InList(
            sub(l),
            items
                .iter()
                .map(|it| substitute_controls(it, num, strv))
                .collect(),
        ),
        Expr::InRange(l, lo, hi) => Expr::InRange(sub(l), sub(lo), sub(hi)),
        Expr::Cond(a, b, c) => Expr::Cond(sub(a), sub(b), sub(c)),
        Expr::Bin(op, a, b) => Expr::Bin(*op, sub(a), sub(b)),
    }
}

/// Lower `e` to a fusevm chunk (with `x` and resolved fields baked in), run it on
/// the VM, and return the resulting number. `resolve` maps a field name to its
/// numeric value.
pub fn eval_ctx(e: &Expr, x: f64, resolve: &dyn Fn(&str) -> f64) -> Result<f64, String> {
    let mut b = ChunkBuilder::new();
    emit(e, x, resolve, &mut b);
    let mut vm = VM::new(b.build());
    match vm.run() {
        VMResult::Ok(v) => Ok(v.to_float()),
        VMResult::Halted => Ok(vm.peek().to_float()),
        VMResult::Error(err) => Err(err),
    }
}

/// Evaluate as a predicate with no field resolver.
pub fn eval_pred(e: &Expr, x: f64) -> Result<bool, String> {
    eval_pred_ctx(e, x, &|_| f64::NAN)
}

/// Evaluate `e` as a predicate — compiled to fusevm, run on the VM, read as a
/// boolean via `Value::is_truthy`. `resolve` maps a field name to its value.
pub fn eval_pred_ctx(e: &Expr, x: f64, resolve: &dyn Fn(&str) -> f64) -> Result<bool, String> {
    let mut b = ChunkBuilder::new();
    emit(e, x, resolve, &mut b);
    let mut vm = VM::new(b.build());
    match vm.run() {
        VMResult::Ok(v) => Ok(v.is_truthy()),
        VMResult::Halted => Ok(vm.peek().is_truthy()),
        VMResult::Error(err) => Err(err),
    }
}

/// Evaluate a `name(args)` inline-Rust FFI call at lowering time: resolve each
/// argument to an f64 on the same VM, marshal it as a `fusevm::Value::float`
/// (fusevm coerces to the export's `i64`/`f64` signature), dispatch to the
/// registered export, and read the return back as an f64. An unregistered name
/// or a failed call yields NaN — the same way an unresolved control degrades —
/// so the emitter can bake the result in as a constant without a fallible path.
fn eval_call(name: &str, args: &[Expr], x: f64, resolve: &dyn Fn(&str) -> f64) -> f64 {
    if !fusevm::ffi::is_registered(name) {
        return f64::NAN;
    }
    let vals: Vec<Value> = args
        .iter()
        .map(|a| Value::float(eval_ctx(a, x, resolve).unwrap_or(f64::NAN)))
        .collect();
    match fusevm::ffi::try_call(name, &vals) {
        Some(Ok(v)) => v.to_float(),
        _ => f64::NAN,
    }
}

fn emit(e: &Expr, x: f64, resolve: &dyn Fn(&str) -> f64, b: &mut ChunkBuilder) {
    match e {
        Expr::Num(n) => {
            b.emit(Op::LoadFloat(*n), 0);
        }
        Expr::Var => {
            b.emit(Op::LoadFloat(x), 0);
        }
        Expr::Field(name) => {
            b.emit(Op::LoadFloat(resolve(name)), 0);
        }
        // An unsubstituted control (its value was empty/non-numeric) -> NaN, so
        // comparisons against it are false. Normally resolve_pipeline replaces it
        // with a Num, or drops the whole `where` op, before we get here.
        Expr::Control(_) => {
            b.emit(Op::LoadFloat(f64::NAN), 0);
        }
        // String predicates never reach the numeric VM — the query layer routes a
        // string-bearing `where` to a Rust evaluator. Emit a neutral 0 so any
        // stray numeric use degrades gracefully instead of panicking.
        Expr::Str(_) | Expr::Match(_) | Expr::InSet(..) => {
            b.emit(Op::LoadFloat(0.0), 0);
        }
        // An FFI call can't run inside fusevm bytecode, so evaluate it now and
        // bake the resulting f64 in as a constant.
        Expr::Call(name, args) => {
            b.emit(Op::LoadFloat(eval_call(name, args, x, resolve)), 0);
        }
        Expr::Neg(a) => {
            emit(a, x, resolve, b);
            b.emit(Op::Negate, 0);
        }
        Expr::Not(a) => {
            // Logical not: `a == 0` yields 1 for falsy `a`, 0 for truthy.
            emit(a, x, resolve, b);
            b.emit(Op::LoadFloat(0.0), 0);
            b.emit(Op::NumEq, 0);
        }
        Expr::InList(left, items) => {
            // Sum of `(left == item)` over the list; truthy iff any matched.
            if items.is_empty() {
                b.emit(Op::LoadFloat(0.0), 0);
            } else {
                for (k, it) in items.iter().enumerate() {
                    emit(left, x, resolve, b);
                    emit(it, x, resolve, b);
                    b.emit(Op::NumEq, 0);
                    if k > 0 {
                        b.emit(Op::Add, 0);
                    }
                }
            }
        }
        Expr::InRange(left, lo, hi) => {
            // `(left >= lo) and (left <= hi)` — both yield 0/1, product is 1 iff
            // in range.
            emit(left, x, resolve, b);
            emit(lo, x, resolve, b);
            b.emit(Op::NumGe, 0);
            emit(left, x, resolve, b);
            emit(hi, x, resolve, b);
            b.emit(Op::NumLe, 0);
            b.emit(Op::Mul, 0);
        }
        Expr::Cond(c, t, e2) => {
            // Real branching: `JumpIfFalse` pops the condition and skips to the
            // else branch when falsy, so only the taken branch runs.
            emit(c, x, resolve, b);
            let jf = b.emit(Op::JumpIfFalse(0), 0);
            emit(t, x, resolve, b);
            let jend = b.emit(Op::Jump(0), 0);
            let else_pos = b.current_pos();
            b.patch_jump(jf, else_pos);
            emit(e2, x, resolve, b);
            let end_pos = b.current_pos();
            b.patch_jump(jend, end_pos);
        }
        // `and`/`or` operate on truthiness: normalize each side to 0/1 first,
        // then `and` = product (1 iff both 1), `or` = sum (>=1 iff either 1).
        Expr::Bin(BinOp::And, a, c) => {
            emit_bool(a, x, resolve, b);
            emit_bool(c, x, resolve, b);
            b.emit(Op::Mul, 0);
        }
        Expr::Bin(BinOp::Or, a, c) => {
            emit_bool(a, x, resolve, b);
            emit_bool(c, x, resolve, b);
            b.emit(Op::Add, 0);
        }
        Expr::Bin(op, a, c) => {
            emit(a, x, resolve, b);
            emit(c, x, resolve, b);
            b.emit(
                match op {
                    BinOp::Add => Op::Add,
                    BinOp::Sub => Op::Sub,
                    BinOp::Mul => Op::Mul,
                    BinOp::Div => Op::Div,
                    BinOp::Mod => Op::Mod,
                    BinOp::Eq => Op::NumEq,
                    BinOp::Ne => Op::NumNe,
                    BinOp::Lt => Op::NumLt,
                    BinOp::Le => Op::NumLe,
                    BinOp::Gt => Op::NumGt,
                    BinOp::Ge => Op::NumGe,
                    BinOp::And | BinOp::Or => unreachable!("handled above"),
                },
                0,
            );
        }
    }
}

/// Emit `e` normalized to a boolean 0.0/1.0 (`e != 0`), for logical combinators.
fn emit_bool(e: &Expr, x: f64, resolve: &dyn Fn(&str) -> f64, b: &mut ChunkBuilder) {
    emit(e, x, resolve, b);
    b.emit(Op::LoadFloat(0.0), 0);
    b.emit(Op::NumNe, 0);
}

struct Parser {
    c: Vec<char>,
    i: usize,
    /// Recursion depth of `primary()`, so a pathologically nested expression
    /// (`(((…x…)))`) fails closed instead of overflowing the stack.
    depth: usize,
}

/// Cap on `(`-nesting in `map`/`where`/`calc` expressions — well past any real
/// expression, but bounded so a malicious input can't abort the process.
const MAX_EXPR_DEPTH: usize = 256;

impl Parser {
    /// Peek the next non-whitespace char (consuming leading whitespace).
    fn peek(&mut self) -> Option<char> {
        while self.i < self.c.len() && self.c[self.i].is_whitespace() {
            self.i += 1;
        }
        self.c.get(self.i).copied()
    }

    /// Lowest precedence: `cond ? then : else` (right-associative).
    fn ternary(&mut self) -> Result<Expr, String> {
        let cond = self.or_expr()?;
        if self.peek() == Some('?') {
            self.i += 1;
            let then = self.ternary()?;
            if self.peek() != Some(':') {
                return Err("calc: expected `:` in `?:`".into());
            }
            self.i += 1;
            let els = self.ternary()?;
            return Ok(Expr::Cond(Box::new(cond), Box::new(then), Box::new(els)));
        }
        Ok(cond)
    }

    /// `a or b`.
    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.and_expr()?;
        while self.match_kw("or") {
            let right = self.and_expr()?;
            left = Expr::Bin(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `a and b`, binding tighter than `or`.
    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.not_expr()?;
        while self.match_kw("and") {
            let right = self.not_expr()?;
            left = Expr::Bin(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Prefix `not`, binding tighter than `and`/`or` but looser than comparison.
    fn not_expr(&mut self) -> Result<Expr, String> {
        if self.match_kw("not") {
            // `not not … 1` recurses here, never through `primary`, so guard it
            // too or a long chain overflows the stack (rc=134).
            self.deepen()?;
            let inner = self.not_expr();
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner?)));
        }
        self.comparison()
    }

    /// Consume the keyword `kw` if it is the next whole word (not a prefix of a
    /// longer identifier, e.g. `and` must not match inside `android`).
    fn match_kw(&mut self, kw: &str) -> bool {
        let save = self.i;
        while self.i < self.c.len() && self.c[self.i].is_whitespace() {
            self.i += 1;
        }
        let start = self.i;
        let kwc: Vec<char> = kw.chars().collect();
        let end = start + kwc.len();
        let boundary_ok =
            end >= self.c.len() || !(self.c[end].is_ascii_alphanumeric() || self.c[end] == '_');
        if end <= self.c.len() && self.c[start..end] == kwc[..] && boundary_ok {
            self.i = end;
            true
        } else {
            self.i = save;
            false
        }
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.additive()?;
        if self.match_kw("in") {
            return self.in_list(left);
        }
        while let Some(op) = self.cmp_op() {
            let right = self.additive()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parse a `.name` control reference into `Expr::Control(name)`.
    fn control_ref(&mut self) -> Result<Expr, String> {
        if self.peek() != Some('.') {
            return Err("expected a control ref `.name`".into());
        }
        self.i += 1; // consume '.'
        let name = self.dotted_ident();
        if name.is_empty() {
            return Err("expected a control name after `.`".into());
        }
        Ok(Expr::Control(name))
    }

    /// Parse the right-hand side of an `in` test: a `.name` control set, a
    /// `[a, b, c]` list, or a `lo..hi` range.
    fn in_list(&mut self, left: Expr) -> Result<Expr, String> {
        // `<field> in .lv` — set membership against a live control's selected set.
        // Only a `.`+letter is a control ref; `.5` stays a fractional range bound.
        if self.peek() == Some('.')
            && matches!(self.c.get(self.i + 1), Some(c) if c.is_ascii_alphabetic() || *c == '_')
        {
            let field = match &left {
                Expr::Field(n) => n.clone(),
                Expr::Var => "x".to_string(),
                _ => return Err("in .set: left side must be a field".into()),
            };
            let ctrl = self.control_ref()?;
            return Ok(Expr::InSet(field, Box::new(ctrl)));
        }
        if self.peek() != Some('[') {
            // `lo..hi` range membership.
            let lo = self.additive()?;
            if !(self.peek() == Some('.') && self.c.get(self.i + 1) == Some(&'.')) {
                return Err("in: expected `[list]` or `lo..hi`".into());
            }
            self.i += 2;
            let hi = self.additive()?;
            return Ok(Expr::InRange(Box::new(left), Box::new(lo), Box::new(hi)));
        }
        self.i += 1;
        let mut items = Vec::new();
        loop {
            if self.peek() == Some(']') {
                self.i += 1;
                break;
            }
            items.push(self.additive()?);
            match self.peek() {
                Some(',') => self.i += 1,
                Some(']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err("in: expected `,` or `]`".into()),
            }
        }
        Ok(Expr::InList(Box::new(left), items))
    }

    /// Consume a comparison operator (`< <= > >= == !=`) if one is next.
    fn cmp_op(&mut self) -> Option<BinOp> {
        let c = self.peek()?;
        if !matches!(c, '<' | '>' | '=' | '!') {
            return None;
        }
        let next = self.c.get(self.i + 1).copied();
        let (op, len) = match (c, next) {
            ('<', Some('=')) => (BinOp::Le, 2),
            ('<', _) => (BinOp::Lt, 1),
            ('>', Some('=')) => (BinOp::Ge, 2),
            ('>', _) => (BinOp::Gt, 1),
            ('=', Some('=')) => (BinOp::Eq, 2),
            ('!', Some('=')) => (BinOp::Ne, 2),
            _ => return None,
        };
        self.i += len;
        Some(op)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.multiplicative()?;
        while let Some(c) = self.peek() {
            let op = match c {
                '+' => BinOp::Add,
                '-' => BinOp::Sub,
                _ => break,
            };
            self.i += 1;
            let right = self.multiplicative()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while let Some(c) = self.peek() {
            let op = match c {
                '*' => BinOp::Mul,
                '/' => BinOp::Div,
                '%' => BinOp::Mod,
                _ => break,
            };
            self.i += 1;
            let right = self.unary()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek() == Some('-') {
            self.i += 1;
            // `- - … 1` recurses here, never through `primary`; guard it too.
            self.deepen()?;
            let inner = self.unary();
            self.depth -= 1;
            return Ok(Expr::Neg(Box::new(inner?)));
        }
        self.primary()
    }

    /// Enter one recursion level, failing closed if it exceeds `MAX_EXPR_DEPTH`
    /// (decrementing itself on that error). Every self-recursive parse point —
    /// `primary` (paren nesting), `not_expr`, `unary` — calls this then does
    /// `self.depth -= 1` after its recursive call, so adversarial input errors
    /// instead of overflowing the stack.
    fn deepen(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err("calc: expression too deeply nested".into());
        }
        Ok(())
    }

    fn primary(&mut self) -> Result<Expr, String> {
        self.deepen()?;
        let r = self.primary_inner();
        self.depth -= 1;
        r
    }

    fn primary_inner(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some('(') => {
                self.i += 1;
                let e = self.additive()?;
                if self.peek() != Some(')') {
                    return Err("calc: expected `)`".into());
                }
                self.i += 1;
                Ok(e)
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let name = self.ident();
                // `match(.q)` — a builtin substring test against a control string.
                if name == "match" && self.peek() == Some('(') {
                    self.i += 1; // consume '('
                    let inner = self.control_ref()?;
                    if self.peek() != Some(')') {
                        return Err("match: expected `)`".into());
                    }
                    self.i += 1;
                    return Ok(Expr::Match(Box::new(inner)));
                }
                // `name(arg, …)` — an inline-Rust FFI call. Any other identifier
                // immediately followed by `(` was previously a parse error, so
                // this grammar addition is purely additive.
                if self.peek() == Some('(') {
                    return self.call(name);
                }
                Ok(if name == "x" {
                    Expr::Var
                } else {
                    Expr::Field(name)
                })
            }
            // `.name` is a control reference (a live widget value); `.5` stays a
            // number. Distinguish by whether the char after `.` is a letter.
            Some('.') if matches!(self.c.get(self.i + 1), Some(c) if c.is_ascii_alphabetic() || *c == '_') =>
            {
                self.i += 1; // consume the leading `.`
                Ok(Expr::Control(self.dotted_ident()))
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) => Err(format!("calc: unexpected `{c}`")),
            None => Err("calc: unexpected end of expression".into()),
        }
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self.i < self.c.len()
            && (self.c[self.i].is_ascii_alphanumeric() || self.c[self.i] == '_')
        {
            self.i += 1;
        }
        self.c[start..self.i].iter().collect()
    }

    /// A control name that may be dot-segmented (`ps.sel`, `g.cpu`): the leading
    /// ident, then any `.segment` where the char after the `.` is a letter/`_` (so
    /// `.5` fractional bounds and `..` ranges are left alone). Enables the
    /// `.<widget>.sel` selection accessor (SPEC §14) as a live control ref.
    fn dotted_ident(&mut self) -> String {
        let mut name = self.ident();
        while self.i < self.c.len()
            && self.c[self.i] == '.'
            && matches!(self.c.get(self.i + 1), Some(c) if c.is_ascii_alphabetic() || *c == '_')
        {
            self.i += 1; // consume '.'
            name.push('.');
            name.push_str(&self.ident());
        }
        name
    }

    /// Parse the argument list of a `name(arg, …)` FFI call — the leading `(` is
    /// the next char. Each argument is a full expression; `,` separates and `)`
    /// closes. A zero-arg call `name()` is allowed.
    fn call(&mut self, name: String) -> Result<Expr, String> {
        self.i += 1; // consume '('
        let mut args = Vec::new();
        if self.peek() != Some(')') {
            loop {
                args.push(self.ternary()?);
                match self.peek() {
                    Some(',') => self.i += 1,
                    Some(')') => break,
                    _ => return Err(format!("calc: expected `,` or `)` in call to `{name}`")),
                }
            }
        }
        self.i += 1; // consume ')'
        Ok(Expr::Call(name, args))
    }

    fn number(&mut self) -> Result<Expr, String> {
        let start = self.i;
        while self.i < self.c.len() && self.c[self.i].is_ascii_digit() {
            self.i += 1;
        }
        // At most one fractional part, and only when the `.` is followed by a
        // digit — so `1..10` (a range) leaves the `..` for the range parser
        // instead of the number greedily swallowing both dots.
        if self.i < self.c.len()
            && self.c[self.i] == '.'
            && matches!(self.c.get(self.i + 1), Some(d) if d.is_ascii_digit())
        {
            self.i += 1;
            while self.i < self.c.len() && self.c[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        let s: String = self.c[start..self.i].iter().collect();
        s.parse::<f64>()
            .map(Expr::Num)
            .map_err(|_| format!("calc: bad number `{s}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_control_name_parses_whole() {
        // `.ps.sel` (a widget's selection accessor) is one control ref, not
        // `.ps` followed by a stray `.sel`.
        let e = parse(".ps.sel").unwrap();
        let mut names = Vec::new();
        control_names(&e, &mut names);
        assert_eq!(names, vec!["ps.sel".to_string()]);
    }

    #[test]
    fn dotted_control_substitutes_and_evals() {
        // `.k.v * 2` with the control resolved to 21 -> 42.
        let e = parse(".k.v * 2").unwrap();
        let num = |n: &str| if n == "k.v" { Some(21.0) } else { None };
        let strv = |_: &str| None;
        let sub = substitute_controls(&e, &num, &strv);
        assert_eq!(eval(&sub, 0.0).unwrap(), 42.0);
    }

    #[test]
    fn fractional_bound_still_a_number_not_a_control() {
        // `.5` stays a number; only `.<letter>` is a control ref.
        assert!(matches!(
            parse("x + .5").unwrap(),
            Expr::Bin(BinOp::Add, _, _)
        ));
    }
}
