//! Which fusevm execution tier a program's bytecode actually reaches.
//!
//! Enabling the JIT is not the same as being compiled by it, and the only
//! honest way to tell the two apart is to ask the VM. This module runs a
//! program and then queries fusevm's own eligibility and cache predicates —
//! `is_block_eligible`, `block_jit_is_compiled`, `trace_is_compiled`,
//! `find_jit_region` — so the answer comes from the compiler that would have
//! done the work rather than from an assumption about it.
//!
//! `arb --tiers 'EXPR'` prints the report.

use std::collections::BTreeMap;

use fusevm::{Chunk, ChunkBuilder, JitCompiler, Op};

/// A loop header — the target of a backward branch — and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    /// Op index of the loop header the backward branch jumps to.
    pub anchor: usize,
    /// Whether fusevm would accept this loop's body as a trace. Asked of
    /// `is_trace_eligible` with the body's ops — the same predicate the
    /// recorder applies to what it recorded, which for a loop whose body has
    /// no early exit is the same op sequence.
    pub trace_eligible: bool,
    /// Whether a compiled trace is installed for this header after the run.
    pub traced: bool,
    /// Whether the tracing JIT gave up on this header.
    pub blacklisted: bool,
}

/// What the tiers did with one chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkTiers {
    /// Which chunk this is — `main` for a whole arb program.
    pub name: String,
    /// Ops in the compiled chunk.
    pub ops: usize,
    /// Whether every op in the chunk is block-JIT eligible, which is what the
    /// whole-chunk block tier requires.
    pub block_eligible: bool,
    /// Whether the block tier holds compiled native code for this chunk.
    pub block_compiled: bool,
    /// The largest contiguous block-eligible op range, if any is large enough
    /// for fusevm to consider it worth compiling.
    pub largest_eligible_region: Option<(usize, usize)>,
    /// Every loop header, and whether the tracing JIT compiled it.
    pub loops: Vec<Loop>,
    /// Op kinds the **block** tier refuses, by occurrence count — what keeps
    /// the whole chunk from being compiled in one piece.
    ///
    /// Not the same question as whether a loop is traced: the tracing tier
    /// takes `GetVar` / `SetVar` (fusevm promotes a referenced global to a
    /// register at trace entry and spills it at every exit), so a chunk can
    /// list those here and still reach native code through a trace.
    pub ineligible: BTreeMap<String, usize>,
}

impl ChunkTiers {
    /// Whether any tier holds compiled native code for this chunk.
    pub fn reaches_native(&self) -> bool {
        self.block_compiled || self.loops.iter().any(|l| l.traced)
    }
}

/// What the tiers did with one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Every chunk the program compiled to, in the order they were lowered.
    pub chunks: Vec<ChunkTiers>,
}

impl Report {
    /// Whether any tier holds compiled native code for any of the program's
    /// chunks.
    pub fn reaches_native(&self) -> bool {
        self.chunks.iter().any(|c| c.reaches_native())
    }
}

impl std::fmt::Display for ChunkTiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "ops                     {}", self.ops)?;
        writeln!(f, "block-JIT eligible      {}", self.block_eligible)?;
        writeln!(f, "block-JIT compiled      {}", self.block_compiled)?;
        match self.largest_eligible_region {
            Some((s, e)) => writeln!(f, "largest eligible region {s}..{e} ({} ops)", e - s)?,
            None => writeln!(f, "largest eligible region none")?,
        }
        if self.loops.is_empty() {
            writeln!(f, "loops                   none")?;
        }
        for l in &self.loops {
            writeln!(
                f,
                "loop @{:<4}             trace-eligible={} traced={} blacklisted={}",
                l.anchor, l.trace_eligible, l.traced, l.blacklisted
            )?;
        }
        if self.ineligible.is_empty() {
            writeln!(f, "block-ineligible ops    none")?;
        } else {
            writeln!(f, "block-ineligible ops")?;
            for (name, count) in &self.ineligible {
                writeln!(f, "  {name:<22}{count}")?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A single-chunk program needs no section headers; the fleet's
        // multi-chunk frontends label each one.
        let label = self.chunks.len() > 1;
        for c in &self.chunks {
            if label {
                writeln!(f, "== {} ==", c.name)?;
            }
            write!(f, "{c}")?;
        }
        write!(f, "reaches native code     {}", self.reaches_native())
    }
}

/// The value of `x` the report evaluates at. Any fixed value does; what matters
/// is that it is the *same* on every warm-up evaluation, so the chunk — and
/// therefore its op hash — stays put long enough for a tier to take it.
const REPORT_X: f64 = 1.0;

/// How many times to evaluate before asking. fusevm's block tier compiles on
/// the second invocation of a chunk (`TraceJitConfig::block_threshold`, default
/// 1), so a handful of evaluations is enough to cross it with margin.
const WARMUP: usize = 8;

/// Lower `expr_src` to its fusevm chunk, evaluate it until the tiers have had
/// their chance, then report which of them took it.
///
/// The expression is evaluated because tier membership is a runtime fact: the
/// block tier compiles only after its warm-up threshold. Nothing is printed by
/// the evaluation itself — an arb expression yields a number, it does not write
/// to stdout — so the report stands alone.
///
/// **What this measures, and what it does not.** arb bakes `x` and every
/// resolved field into the chunk as `LoadFloat` constants
/// (`expr::chunk_of`), so an expression evaluated over a pipeline produces a
/// *different chunk per row* — different constants, different op hash, and
/// therefore a warm-up counter that restarts from zero every row. This report
/// holds `x` fixed at `REPORT_X` so a single chunk survives long enough to be
/// compiled, which answers "is this expression's shape something the tiers can
/// take" but not "does a running pipeline reach native code". The second
/// question's answer is no as long as the constants are baked per evaluation.
pub fn report(expr_src: &str) -> Result<Report, String> {
    let e = crate::expr::parse(expr_src)?;
    let resolve = |_: &str| f64::NAN;
    let chunk = crate::expr::chunk_of(&e, REPORT_X, &resolve);
    for _ in 0..WARMUP {
        crate::expr::eval_ctx(&e, REPORT_X, &resolve)?;
    }
    Ok(inspect_all(&[("expr".to_string(), chunk)]))
}

/// Report on one already-executed chunk, as a whole-program report. Used by
/// tests that build a chunk by hand.
pub fn inspect(chunk: &Chunk) -> Report {
    Report {
        chunks: vec![inspect_chunk("main", chunk)],
    }
}

/// Report on every chunk a compiled program holds, in lowering order.
pub fn inspect_all(named: &[(String, Chunk)]) -> Report {
    Report {
        chunks: named.iter().map(|(n, c)| inspect_chunk(n, c)).collect(),
    }
}

/// Report on one already-executed chunk.
pub fn inspect_chunk(name: &str, chunk: &Chunk) -> ChunkTiers {
    let jit = JitCompiler::new();
    let loops = loop_anchors(&chunk.ops)
        .into_iter()
        .map(|anchor| Loop {
            anchor,
            trace_eligible: body_of(&chunk.ops, anchor)
                .is_some_and(|body| jit.is_trace_eligible(body, anchor)),
            traced: jit.trace_is_compiled(chunk, anchor),
            blacklisted: jit.trace_is_blacklisted(chunk, anchor),
        })
        .collect();

    let mut ineligible: BTreeMap<String, usize> = BTreeMap::new();
    for op in &chunk.ops {
        if !op_is_eligible(&jit, op) {
            *ineligible.entry(op_name(op)).or_default() += 1;
        }
    }

    ChunkTiers {
        name: name.to_string(),
        ops: chunk.ops.len(),
        block_eligible: jit.is_block_eligible(chunk),
        block_compiled: jit.block_jit_is_compiled(chunk),
        largest_eligible_region: jit.find_jit_region(chunk),
        loops,
        ineligible,
    }
}

/// Every op index a backward branch jumps to — fusevm anchors a trace at each.
fn loop_anchors(ops: &[Op]) -> Vec<usize> {
    let mut anchors: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(ip, op)| match op {
            Op::Jump(t)
            | Op::JumpIfTrue(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfTrueKeep(t)
            | Op::JumpIfFalseKeep(t)
                if *t <= ip =>
            {
                Some(*t)
            }
            _ => None,
        })
        .collect();
    anchors.sort_unstable();
    anchors.dedup();
    anchors
}

/// The op sequence one iteration of the loop at `anchor` runs: from the header
/// through the backward branch that closes it. `None` when nothing closes it.
fn body_of(ops: &[Op], anchor: usize) -> Option<&[Op]> {
    let close = ops.iter().enumerate().position(|(ip, op)| {
        ip >= anchor
            && matches!(
                op,
                Op::Jump(t) | Op::JumpIfTrue(t) | Op::JumpIfFalse(t)
                    if *t == anchor
            )
    })?;
    Some(&ops[anchor..=close])
}

/// Whether fusevm's block tier accepts this op, asked by handing the JIT a
/// chunk holding just that op. Whole-chunk eligibility is the conjunction of
/// the per-op decision, so a one-op chunk isolates it.
fn op_is_eligible(jit: &JitCompiler, op: &Op) -> bool {
    let mut b = ChunkBuilder::new();
    b.emit(op.clone(), 1);
    jit.is_block_eligible(&b.build())
}

/// An op's variant name, without its operands, so occurrences group.
fn op_name(op: &Op) -> String {
    let text = format!("{op:?}");
    match text.split_once('(') {
        Some((name, _)) => name.to_string(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arb expression is exactly what the block tier is for: straight-line
    /// arithmetic over baked-in constants, no branch, no call. Evaluated past
    /// the warm-up threshold it is compiled, and the report says so.
    ///
    /// This is also the test that fails if arb ever stops arming the tiers:
    /// fusevm gates the *block* tier behind the same `tracing_jit` flag as the
    /// tracing tier (`vm.rs`'s tiered auto-dispatch checks it before it will
    /// even ask `is_block_eligible`), so a VM built without `arm_tiers` reports
    /// `block-JIT compiled false` no matter how eligible the chunk is.
    #[test]
    fn an_arithmetic_expression_is_compiled_by_the_block_tier() {
        let report = report("x * 2 + 3 * 4 - 1").expect("evaluates");
        assert!(report.chunks[0].block_eligible, "{report}");
        assert!(report.chunks[0].block_compiled, "{report}");
        assert!(report.chunks[0].ineligible.is_empty(), "{report}");
        assert!(report.reaches_native(), "{report}");
    }

    /// A guarded ternary branches, and the branch is still block-eligible —
    /// the tier takes real control flow, not just flat arithmetic.
    #[test]
    fn a_guarded_ternary_is_still_block_eligible() {
        let report = report("x != 0 ? 100 / x : 0").expect("evaluates");
        assert!(report.chunks[0].block_eligible, "{report}");
        assert!(report.chunks[0].loops.is_empty(), "{report}");
        assert!(report.reaches_native(), "{report}");
    }

    /// The constants are baked per evaluation, so the same expression at a
    /// different `x` is a *different* chunk with a different op hash. That is
    /// why the report holds `x` fixed, and why a pipeline — which varies `x`
    /// per row — restarts the warm-up counter on every row and never reaches
    /// the tier this report shows it could reach. Pins the fact so that if the
    /// lowering ever moves `x` into a slot, this test says so.
    #[test]
    fn a_different_x_is_a_different_chunk() {
        let e = crate::expr::parse("x * 2 + 1").expect("parses");
        let resolve = |_: &str| f64::NAN;
        let a = crate::expr::chunk_of(&e, 1.0, &resolve);
        let b = crate::expr::chunk_of(&e, 2.0, &resolve);
        assert_eq!(a.ops.len(), b.ops.len(), "same shape");
        assert_ne!(
            a.op_hash, b.op_hash,
            "x is baked in as a constant, so the chunks cannot share a cache key"
        );
    }
}
