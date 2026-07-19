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
use fusevm::{ChunkBuilder, Op};

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
    Neg(Box<Expr>),
    /// Logical negation: truthy input -> 0, falsy -> 1.
    Not(Box<Expr>),
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
        Expr::Num(_) | Expr::Var | Expr::Field(_) => {}
        Expr::Neg(a) | Expr::Not(a) => control_names(a, out),
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

/// Return a copy of `e` with each `Control(name)` replaced by `Num(v)` when
/// `lookup(name)` yields a value; unresolved controls are left as-is (-> NaN at
/// emit time).
pub fn substitute_controls(e: &Expr, lookup: &dyn Fn(&str) -> Option<f64>) -> Expr {
    let sub = |b: &Expr| Box::new(substitute_controls(b, lookup));
    match e {
        Expr::Control(n) => match lookup(n) {
            Some(v) => Expr::Num(v),
            None => Expr::Control(n.clone()),
        },
        Expr::Num(_) | Expr::Var | Expr::Field(_) => e.clone(),
        Expr::Neg(a) => Expr::Neg(sub(a)),
        Expr::Not(a) => Expr::Not(sub(a)),
        Expr::InList(l, items) => Expr::InList(
            sub(l),
            items.iter().map(|it| substitute_controls(it, lookup)).collect(),
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
}

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
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
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
        let boundary_ok = end >= self.c.len()
            || !(self.c[end].is_ascii_alphanumeric() || self.c[end] == '_');
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

    /// Parse the right-hand side of an `in` test: either a `[a, b, c]` list or a
    /// `lo..hi` range.
    fn in_list(&mut self, left: Expr) -> Result<Expr, String> {
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
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
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
                Ok(if name == "x" {
                    Expr::Var
                } else {
                    Expr::Field(name)
                })
            }
            // `.name` is a control reference (a live widget value); `.5` stays a
            // number. Distinguish by whether the char after `.` is a letter.
            Some('.')
                if matches!(self.c.get(self.i + 1), Some(c) if c.is_ascii_alphabetic() || *c == '_') =>
            {
                self.i += 1; // consume the leading `.`
                Ok(Expr::Control(self.ident()))
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
