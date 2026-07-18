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
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    /// The pipeline input scalar.
    Var,
    /// A named field of the current record, resolved at eval time.
    Field(String),
    Neg(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
}

/// Parse an expression: numbers, `x`, `+ - * / %`, comparisons
/// (`== != < <= > >=`), unary `-`, and parentheses.
pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = Parser {
        c: src.chars().collect(),
        i: 0,
    };
    let e = p.comparison()?;
    if p.peek().is_some() {
        return Err(format!("calc: unexpected `{}`", p.c[p.i]));
    }
    Ok(e)
}

/// Evaluate as a number with no field resolver (field refs -> NaN).
pub fn eval(e: &Expr, x: f64) -> Result<f64, String> {
    eval_ctx(e, x, &|_| f64::NAN)
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
        Expr::Neg(a) => {
            emit(a, x, resolve, b);
            b.emit(Op::Negate, 0);
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
                },
                0,
            );
        }
    }
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

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.additive()?;
        while let Some(op) = self.cmp_op() {
            let right = self.additive()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
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
        while self.i < self.c.len() && (self.c[self.i].is_ascii_digit() || self.c[self.i] == '.') {
            self.i += 1;
        }
        let s: String = self.c[start..self.i].iter().collect();
        s.parse::<f64>()
            .map(Expr::Num)
            .map_err(|_| format!("calc: bad number `{s}`"))
    }
}
