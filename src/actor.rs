//! Actor system — Akka-style message-passing concurrency for arb.
//!
//! An `actor NAME(state) { on MSG(p…) { … } }` declaration compiles to an
//! [`ActorDef`]: a named, single-`state` scalar behavior with one handler per
//! message. Handlers run arb expressions (lowered to fusevm, same core as
//! `map`/`where`/`calc`) over `state`, the message parameters, and any locals
//! they assign; a `reply EXPR` sends a value back to an `ask`/`via` caller.
//!
//! The runtime is real OS threads with `mpsc` mailboxes — one thread per spawned
//! actor, blocking on its receiver:
//!   * [`ActorSystem::spawn`] starts an actor; its [`ActorRef`] holds the sender.
//!   * [`ActorRef::send`] is *tell* (fire-and-forget).
//!   * [`ActorRef::ask`] is *ask* (posts a one-shot reply channel, blocks for it).
//!   * [`ActorSystem::pool`] is a supervised round-robin pool; a worker whose
//!     mailbox has died (thread gone) is respawned on the next dispatch.
//!
//! [`run_via`] is the pipeline-facing consumer: it fans a stream of scalars
//! across a pool in parallel (`source .out { in; via NAME * 8 }`), each line
//! becoming a `job(x)` ask whose reply is the output line, order preserved.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::expr::{self, Expr};

/// One statement in a handler body: assign a variable, or reply to the caller.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// `var = EXPR` — set `state`, a parameter, or a local to the value of EXPR.
    Assign(String, Expr),
    /// `reply EXPR` — send EXPR's value back on the message's reply channel.
    Reply(Expr),
}

/// A message handler: `on MSG(p1, p2) { body }`.
#[derive(Debug, Clone)]
pub struct Handler {
    pub msg: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// A compiled actor behavior. `state_param` is the single scalar carried across
/// messages (initialized by `spawn`, default 0). Handlers are matched by message
/// name; the first handler is the default target for `via`.
#[derive(Debug, Clone)]
pub struct ActorDef {
    pub name: String,
    pub state_param: Option<String>,
    pub handlers: Vec<Handler>,
}

impl ActorDef {
    /// Find the handler for message `msg`.
    fn handler(&self, msg: &str) -> Option<&Handler> {
        self.handlers.iter().find(|h| h.msg == msg)
    }
}

// ---- parsing (from the command tree) --------------------------------------

/// Parse an `actor NAME(state) { on … }` command into an [`ActorDef`].
///
/// `args[0]` is the header word `NAME(state)` / `NAME`; `args[1]` is the handler
/// block. Each inner command is `on MSG(params) { body }`.
pub fn parse_actor(args: &[crate::ast::Arg]) -> Result<ActorDef, String> {
    use crate::ast::Arg;
    let header = args
        .first()
        .and_then(Arg::as_str)
        .ok_or("actor: expected `actor NAME(state) { … }`")?;
    let (name, state_param) = split_header(header)?;
    if name.is_empty() {
        return Err("actor: missing name".into());
    }
    let block = match args.get(1) {
        Some(Arg::Block(cmds)) => cmds,
        _ => return Err("actor: expected `{ on MSG { … } }` handler block".into()),
    };
    let mut handlers = Vec::new();
    for c in block {
        if c.name != "on" {
            return Err(format!("actor: expected `on MSG {{ … }}`, got `{}`", c.name));
        }
        let sig = c
            .args
            .first()
            .and_then(Arg::as_str)
            .ok_or("actor: `on` needs a message name")?;
        let (msg, params) = split_call(sig);
        let body_cmds = match c.args.get(1) {
            Some(Arg::Block(b)) => b,
            _ => return Err(format!("actor: `on {msg}` needs a `{{ … }}` body")),
        };
        // Extra args after the body mean two handlers ran together on one line
        // (block tokens don't end a command) — flag it rather than silently
        // dropping the rest.
        if c.args.len() > 2 {
            return Err(format!(
                "actor: `on {msg}` has trailing tokens — separate handlers by a newline or `;`"
            ));
        }
        handlers.push(Handler {
            msg,
            params,
            body: parse_body(body_cmds)?,
        });
    }
    if handlers.is_empty() {
        return Err("actor: needs at least one `on MSG { … }` handler".into());
    }
    Ok(ActorDef {
        name,
        state_param,
        handlers,
    })
}

/// Split `NAME(state)` / `NAME()` / `NAME` into (name, optional state param).
fn split_header(h: &str) -> Result<(String, Option<String>), String> {
    let (name, params) = split_call(h);
    match params.len() {
        0 => Ok((name, None)),
        1 => Ok((name, Some(params.into_iter().next().unwrap()))),
        _ => Err(format!("actor: `{name}` takes a single state parameter")),
    }
}

/// Split `NAME(a, b, c)` into ("NAME", ["a","b","c"]); `NAME` -> ("NAME", []).
fn split_call(s: &str) -> (String, Vec<String>) {
    let s = s.trim();
    match s.find('(') {
        None => (s.to_string(), Vec::new()),
        Some(i) => {
            let name = s[..i].trim().to_string();
            let inner = s[i + 1..].trim_end_matches(')');
            let params = inner
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect();
            (name, params)
        }
    }
}

/// Parse a handler body: each command is `reply EXPR` or `var = EXPR`.
fn parse_body(cmds: &[crate::ast::Command]) -> Result<Vec<Stmt>, String> {
    let mut out = Vec::with_capacity(cmds.len());
    for c in cmds {
        if c.name == "reply" {
            out.push(Stmt::Reply(parse_expr_args(&c.args)?));
        } else if c.args.first().and_then(crate::ast::Arg::as_str) == Some("=") {
            // Spaced `var = EXPR`: the verb is the target, `=` its first arg.
            out.push(Stmt::Assign(c.name.clone(), parse_expr_args(&c.args[1..])?));
        } else if let Some((lhs, rhs)) = c.name.split_once('=') {
            // Unspaced `var=EXPR` / `var=EXPR...`: the lexer keeps `var=...` as one
            // word. Split at the first `=`; the RHS continues with any later args.
            let var = lhs.trim();
            if var.is_empty() || !var.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
                return Err(format!("actor: `{}` is not a valid assignment target", c.name));
            }
            let mut expr = rhs.trim().to_string();
            for a in &c.args {
                let s = a
                    .as_str()
                    .ok_or("actor: a `{ … }` block is not an expression")?;
                expr.push(' ');
                expr.push_str(s);
            }
            out.push(Stmt::Assign(var.to_string(), expr::parse(expr.trim())?));
        } else {
            return Err(format!(
                "actor: handler statement must be `reply EXPR` or `var = EXPR`, got `{}`",
                c.name
            ));
        }
    }
    Ok(out)
}

/// Reconstruct an expression string from word args and parse it via [`expr`].
fn parse_expr_args(args: &[crate::ast::Arg]) -> Result<Expr, String> {
    let mut parts = Vec::with_capacity(args.len());
    for a in args {
        match a.as_str() {
            Some(s) => parts.push(s),
            None => return Err("actor: a `{ … }` block is not an expression".into()),
        }
    }
    if parts.is_empty() {
        return Err("actor: empty expression".into());
    }
    expr::parse(&parts.join(" "))
}

// ---- handler evaluation ----------------------------------------------------

/// Evaluate a handler against its bound environment, returning the last `reply`
/// value if any. `env` starts with `state` + parameters bound; assignments
/// mutate it (so `state` persists across the caller's uses).
fn run_handler(h: &Handler, env: &mut HashMap<String, f64>) -> Option<f64> {
    let mut reply = None;
    for st in &h.body {
        match st {
            Stmt::Assign(name, e) => {
                let v = eval_env(e, env);
                env.insert(name.clone(), v);
            }
            Stmt::Reply(e) => reply = Some(eval_env(e, env)),
        }
    }
    reply
}

/// Evaluate `e` with barewords resolved from `env` (unknown -> 0). `x`
/// (`Expr::Var`) reads the variable literally named `x`, so `on job(x)` binds it.
fn eval_env(e: &Expr, env: &HashMap<String, f64>) -> f64 {
    let x = env.get("x").copied().unwrap_or(0.0);
    let resolve = |name: &str| env.get(name).copied().unwrap_or(0.0);
    expr::eval_ctx(e, x, &resolve).unwrap_or(f64::NAN)
}

// ---- runtime: mailboxes, spawn/send/ask -----------------------------------

/// A message posted to an actor: a name, positional scalar args, and an optional
/// one-shot reply channel (present for `ask`, absent for `send`).
struct Message {
    name: String,
    args: Vec<f64>,
    reply: Option<Sender<f64>>,
}

/// A handle to a running actor: the sending half of its mailbox. Cloning shares
/// one mailbox (every clone posts to the same thread).
#[derive(Clone)]
pub struct ActorRef {
    tx: Sender<Message>,
}

impl ActorRef {
    /// *Tell*: post a message, fire-and-forget. Returns `false` if the actor is
    /// already gone.
    pub fn send(&self, msg: &str, args: Vec<f64>) -> bool {
        self.tx
            .send(Message {
                name: msg.to_string(),
                args,
                reply: None,
            })
            .is_ok()
    }

    /// *Ask*: post a message with a reply channel and block for the handler's
    /// `reply`. `None` if the actor died or the handler produced no reply.
    pub fn ask(&self, msg: &str, args: Vec<f64>) -> Option<f64> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(Message {
                name: msg.to_string(),
                args,
                reply: Some(rtx),
            })
            .ok()?;
        rrx.recv().ok()
    }
}

/// Owns spawned actor threads and joins them on drop.
#[derive(Default)]
pub struct ActorSystem {
    handles: Vec<JoinHandle<()>>,
}

impl ActorSystem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `def` with initial state `init`, returning a ref to its mailbox.
    /// One OS thread blocks on the mailbox and dispatches each message to the
    /// matching handler, carrying `state` across messages.
    pub fn spawn(&mut self, def: Arc<ActorDef>, init: f64) -> ActorRef {
        let (tx, rx) = mpsc::channel::<Message>();
        let handle = std::thread::spawn(move || actor_loop(&def, init, rx));
        self.handles.push(handle);
        ActorRef { tx }
    }

    /// A supervised round-robin pool of `n` copies of `def`. Dispatch (`ask`)
    /// rotates across workers; a worker whose thread has died is respawned.
    pub fn pool(&mut self, def: Arc<ActorDef>, n: usize, init: f64) -> Pool {
        let n = n.max(1);
        let refs = (0..n).map(|_| self.spawn(def.clone(), init)).collect();
        Pool {
            def,
            init,
            refs: Mutex::new(refs),
            next: Mutex::new(0),
        }
    }
}

impl Drop for ActorSystem {
    fn drop(&mut self) {
        // Threads own their receivers; when every ActorRef is dropped the sender
        // side closes and `recv` returns Err, ending the loop. Join to be tidy.
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// The per-actor dispatch loop. `catch_unwind` around each handler means a
/// panicking message resets the actor's state (supervision: restart) instead of
/// killing the thread and orphaning the mailbox.
fn actor_loop(def: &ActorDef, init: f64, rx: Receiver<Message>) {
    let mut state = init;
    while let Ok(msg) = rx.recv() {
        let Some(h) = def.handler(&msg.name) else {
            // Unknown message: reply with the current state so an `ask` never hangs.
            if let Some(r) = msg.reply {
                let _ = r.send(state);
            }
            continue;
        };
        let mut env_map: HashMap<String, f64> = HashMap::new();
        if let Some(sp) = &def.state_param {
            env_map.insert(sp.clone(), state);
        }
        for (p, v) in h.params.iter().zip(msg.args.iter()) {
            env_map.insert(p.clone(), *v);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_handler(h, &mut env_map)
        }));
        match result {
            Ok(reply) => {
                // Persist the (possibly reassigned) state for the next message.
                if let Some(sp) = &def.state_param {
                    if let Some(v) = env_map.get(sp) {
                        state = *v;
                    }
                }
                if let Some(r) = msg.reply {
                    // No explicit reply -> hand back the current state.
                    let _ = r.send(reply.unwrap_or(state));
                }
            }
            Err(_) => {
                // Handler panicked: restart — reset state, answer with it so the
                // caller is not left blocked.
                state = init;
                if let Some(r) = msg.reply {
                    let _ = r.send(state);
                }
            }
        }
    }
}

/// A supervised round-robin actor pool.
pub struct Pool {
    def: Arc<ActorDef>,
    init: f64,
    refs: Mutex<Vec<ActorRef>>,
    next: Mutex<usize>,
}

impl Pool {
    /// Number of workers.
    pub fn size(&self) -> usize {
        self.refs.lock().unwrap().len()
    }

    /// Ask the next worker in rotation; respawn-and-retry once if it is dead.
    /// Requires the owning [`ActorSystem`] so a respawned worker's thread is
    /// tracked and joined like the rest.
    pub fn ask(&self, sys: &Mutex<ActorSystem>, msg: &str, args: Vec<f64>) -> Option<f64> {
        // Pick a worker and clone its cheap sender handle out from under the lock,
        // so the blocking `ask` runs without serializing the whole pool.
        let (idx, worker) = {
            let refs = self.refs.lock().unwrap();
            let len = refs.len().max(1);
            let mut n = self.next.lock().unwrap();
            let i = *n % len;
            *n = i + 1;
            (i, refs.get(i).cloned())
        };
        if let Some(w) = worker {
            if let Some(v) = w.ask(msg, args.clone()) {
                return Some(v);
            }
        }
        // Dead worker: respawn its slot and retry.
        let fresh = sys.lock().unwrap().spawn(self.def.clone(), self.init);
        let v = fresh.ask(msg, args);
        if let Some(slot) = self.refs.lock().unwrap().get_mut(idx) {
            *slot = fresh;
        }
        v
    }
}

// ---- pipeline fan-out: `via NAME [* N]` ------------------------------------

/// Default `via` pool width when unspecified — one worker per hardware thread.
fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Fan `lines` across a pool of `def` and collect the replies, order preserved.
///
/// Each line's scalar (`crate::spec::parse_scalar`) is asked as the first
/// handler's message, binding parameter 0; the reply formats to the output line.
/// A pure-map handler (`reply x * 2`) is deterministic regardless of pool width;
/// a stateful handler is partitioned across workers (documented in SPEC §15).
pub fn run_via(def: &Arc<ActorDef>, workers: usize, lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let n = if workers == 0 {
        default_workers()
    } else {
        workers
    }
    .min(lines.len().max(1));
    let msg = def.handlers[0].msg.clone();
    let reply = |l: &String, v: Option<f64>| match v {
        Some(v) => crate::query::fmt_scalar(v),
        None => l.clone(),
    };

    // A single worker is a *sequential* actor: dispatch in input order so its
    // `state` accumulates deterministically (`via NAME * 1` = one mailbox, one
    // ordered stream). Concurrent dispatch would let messages arrive out of
    // order, so only the multi-worker map path fans out in parallel.
    let mut sys = ActorSystem::new();
    if n == 1 {
        let r = sys.spawn(def.clone(), 0.0);
        return lines
            .iter()
            .map(|l| reply(l, r.ask(&msg, vec![crate::spec::parse_scalar(l)])))
            .collect();
    }

    // Multi-worker pool: each line is asked concurrently (rayon), the pool's
    // round-robin spreads asks across workers, and `par_iter().collect()` keeps
    // output order matching the input. A pure-map handler is deterministic; a
    // stateful handler is partitioned across workers (SPEC §15).
    let sys = Mutex::new(sys);
    let pool = {
        let mut s = sys.lock().unwrap();
        s.pool(def.clone(), n, 0.0)
    };
    use rayon::prelude::*;
    lines
        .par_iter()
        .map(|l| reply(l, pool.ask(&sys, &msg, vec![crate::spec::parse_scalar(l)])))
        .collect()
}

// ---- imperative session: spawn / pool / tell / ask / supervise -------------

/// A top-level actor binding: `spawn NAME = ACTOR(init)` or
/// `pool NAME = ACTOR * N`, optionally re-tuned by `supervise NAME { … }`.
#[derive(Debug, Clone)]
pub struct RefDecl {
    /// The binding name used by `tell`/`ask` (`w`, `p`).
    pub name: String,
    /// The `actor NAME(state) { … }` this ref runs.
    pub actor: String,
    /// Initial `state`, evaluated at build time (`spawn w = worker(3)` -> 3).
    pub init: f64,
    /// `Some(N)` for a `pool … * N`; `None` for a single actor.
    pub pool: Option<usize>,
    /// Supervision policy: respawn a dead worker (default), or fail-stop when a
    /// `supervise NAME { on crash { stop } }` clause turned it off.
    pub restart: bool,
}

/// A named ref inside a live [`Session`]: one actor, or a supervised pool.
enum Target {
    One(ActorRef),
    Pool {
        workers: Vec<ActorRef>,
        next: usize,
        def: Arc<ActorDef>,
        init: f64,
        restart: bool,
    },
}

/// The interactive actor runtime for a spec: owns the [`ActorSystem`] and every
/// `spawn`/`pool` binding, so `tell`/`ask` bind actions can drive them across the
/// session (SPEC §15). Single-threaded access from the TUI loop (`&mut self`).
#[derive(Default)]
pub struct Session {
    // Field order is load-bearing: `refs` (which holds the mailbox senders) MUST
    // drop before `sys` (whose Drop joins the threads). Struct fields drop in
    // declaration order, so refs first closes the senders, letting each `recv`
    // return Err and the thread exit — otherwise `sys`'s join would hang forever.
    refs: HashMap<String, Target>,
    sys: ActorSystem,
}

impl Session {
    /// Spawn every declared ref against the actor registry. A ref naming an
    /// unknown actor is an error (surfaced at build, not silently dropped).
    pub fn build(
        defs: &std::collections::BTreeMap<String, Arc<ActorDef>>,
        decls: &[RefDecl],
    ) -> Result<Session, String> {
        let mut s = Session::default();
        for d in decls {
            let def = defs
                .get(&d.actor)
                .ok_or_else(|| format!("spawn: no actor named `{}`", d.actor))?
                .clone();
            let target = match d.pool {
                Some(n) => {
                    let n = n.max(1);
                    Target::Pool {
                        workers: (0..n).map(|_| s.sys.spawn(def.clone(), d.init)).collect(),
                        next: 0,
                        def,
                        init: d.init,
                        restart: d.restart,
                    }
                }
                None => Target::One(s.sys.spawn(def, d.init)),
            };
            s.refs.insert(d.name.clone(), target);
        }
        Ok(s)
    }

    /// Whether the session has any refs (an all-`via` spec has none).
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Pick the next live worker for a ref, respawning a dead one when the ref is
    /// supervised. Returns a cloned mailbox handle (cheap) to send on.
    fn worker(&mut self, name: &str) -> Option<ActorRef> {
        match self.refs.get_mut(name)? {
            Target::One(r) => Some(r.clone()),
            Target::Pool {
                workers,
                next,
                def,
                init,
                restart,
            } => {
                if workers.is_empty() {
                    return None;
                }
                let i = *next % workers.len();
                *next = i + 1;
                // A supervised pool respawns a worker whose thread has died before
                // dispatching to it; an unsupervised (fail-stop) pool does not.
                if *restart {
                    let probe = mpsc::channel::<f64>().0;
                    let dead = workers[i]
                        .tx
                        .send(Message {
                            name: String::new(),
                            args: Vec::new(),
                            reply: Some(probe),
                        })
                        .is_err();
                    if dead {
                        workers[i] = self.sys.spawn(def.clone(), *init);
                    }
                }
                Some(workers[i].clone())
            }
        }
    }

    /// *Tell* a ref (fire-and-forget). Unknown ref -> ignored.
    pub fn tell(&mut self, name: &str, msg: &str, args: Vec<f64>) {
        if let Some(w) = self.worker(name) {
            w.send(msg, args);
        }
    }

    /// *Ask* a ref and block for the reply. Unknown/dead ref -> `None`.
    pub fn ask(&mut self, name: &str, msg: &str, args: Vec<f64>) -> Option<f64> {
        self.worker(name).and_then(|w| w.ask(msg, args))
    }
}

/// Parse a message call `MSG(a, b, …)` (or bare `MSG`) into its name and the
/// argument expressions. Commas split only at paren depth 0, so an arithmetic
/// arg like `(a + b) * 2` stays intact.
pub fn parse_call(s: &str) -> Result<(String, Vec<Expr>), String> {
    let s = s.trim();
    let open = match s.find('(') {
        None => return Ok((s.to_string(), Vec::new())),
        Some(i) => i,
    };
    let name = s[..open].trim().to_string();
    if name.is_empty() {
        return Err("message call: missing name".into());
    }
    let inner = s
        .strip_suffix(')')
        .and_then(|s| s.get(open + 1..))
        .ok_or("message call: unbalanced `()`")?;
    let mut args = Vec::new();
    let (mut depth, mut start) = (0isize, 0usize);
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                push_arg(&inner[start..i], &mut args)?;
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner.trim().is_empty() {
        push_arg(&inner[start..], &mut args)?;
    }
    Ok((name, args))
}

fn push_arg(raw: &str, out: &mut Vec<Expr>) -> Result<(), String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("message call: empty argument".into());
    }
    out.push(expr::parse(t)?);
    Ok(())
}

/// Evaluate a message call's argument expressions against live control values —
/// a `.name` control resolves to its current numeric input (unset -> NaN).
pub fn eval_args(exprs: &[Expr], inputs: &HashMap<String, String>) -> Vec<f64> {
    let num = |n: &str| -> Option<f64> { inputs.get(n).and_then(|v| v.trim().parse::<f64>().ok()) };
    let strv = |n: &str| -> Option<String> { inputs.get(n).cloned() };
    exprs
        .iter()
        .map(|e| {
            let sub = expr::substitute_controls(e, &num, &strv);
            expr::eval(&sub, 0.0).unwrap_or(f64::NAN)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn def(src: &str) -> Arc<ActorDef> {
        let cmds = parse(src).unwrap();
        Arc::new(parse_actor(&cmds[0].args).unwrap())
    }

    #[test]
    fn parses_header_and_handlers() {
        // Handlers are separated like every other verb — by newline or `;`.
        let d = def("actor worker(state) {\n on job(x) { reply x * 2 }\n on reset { state = 0 }\n }");
        assert_eq!(d.name, "worker");
        assert_eq!(d.state_param.as_deref(), Some("state"));
        assert_eq!(d.handlers.len(), 2);
        assert_eq!(d.handlers[0].msg, "job");
        assert_eq!(d.handlers[0].params, vec!["x".to_string()]);
        assert_eq!(d.handlers[1].msg, "reset");
        assert!(d.handlers[1].params.is_empty());
    }

    #[test]
    fn unspaced_assignment_is_accepted() {
        // The lexer keeps `state=state+1` as one word; parse_body splits it.
        let d = def("actor c(state) { on tick { state=state+1; reply state } }");
        let mut sys = ActorSystem::new();
        let r = sys.spawn(d, 0.0);
        assert_eq!(r.ask("tick", vec![]), Some(1.0));
        assert_eq!(r.ask("tick", vec![]), Some(2.0));
    }

    #[test]
    fn header_without_state_is_allowed() {
        let d = def("actor doubler { on job(x) { reply x + x } }");
        assert!(d.state_param.is_none());
    }

    #[test]
    fn spawn_send_ask_roundtrip() {
        let d = def("actor w(state) { on job(x) { reply x * 3 } }");
        let mut sys = ActorSystem::new();
        let r = sys.spawn(d, 0.0);
        assert_eq!(r.ask("job", vec![7.0]), Some(21.0));
    }

    #[test]
    fn state_persists_across_messages() {
        // `add(n)` accumulates into state and replies the running total.
        let d = def("actor acc(state) { on add(n) { state = state + n; reply state } }");
        let mut sys = ActorSystem::new();
        let r = sys.spawn(d, 0.0);
        assert_eq!(r.ask("add", vec![10.0]), Some(10.0));
        assert_eq!(r.ask("add", vec![5.0]), Some(15.0));
        assert_eq!(r.ask("add", vec![100.0]), Some(115.0));
    }

    #[test]
    fn tell_is_fire_and_forget() {
        let d = def("actor sink(state) { on set(v) { state = v; reply state } }");
        let mut sys = ActorSystem::new();
        let r = sys.spawn(d, 0.0);
        assert!(r.send("set", vec![42.0]));
        // A following ask observes the tell's state mutation (same mailbox order).
        assert_eq!(r.ask("set", vec![9.0]), Some(9.0));
    }

    #[test]
    fn unknown_message_replies_state_not_hang() {
        let d = def("actor w(state) { on job(x) { reply x } }");
        let mut sys = ActorSystem::new();
        let r = sys.spawn(d, 3.0);
        assert_eq!(r.ask("nope", vec![]), Some(3.0));
    }

    #[test]
    fn via_is_a_deterministic_parallel_map() {
        let d = def("actor sq(state) { on job(x) { reply x * x } }");
        let lines: Vec<String> = (1..=6).map(|i| i.to_string()).collect();
        let out = run_via(&d, 4, &lines);
        assert_eq!(out, vec!["1", "4", "9", "16", "25", "36"]);
    }

    #[test]
    fn via_preserves_order_with_single_worker() {
        let d = def("actor inc(state) { on job(x) { reply x + 1 } }");
        let lines: Vec<String> = vec!["10".into(), "20".into(), "30".into()];
        assert_eq!(run_via(&d, 1, &lines), vec!["11", "21", "31"]);
    }

    #[test]
    fn via_single_worker_is_a_sequential_accumulator() {
        // `via NAME * 1` dispatches in input order, so state accumulates
        // deterministically — a running total, not a per-worker partition.
        let d = def("actor acc(state) { on job(x) { state = state + x; reply state } }");
        let lines: Vec<String> = vec!["10".into(), "5".into(), "100".into()];
        assert_eq!(run_via(&d, 1, &lines), vec!["10", "15", "115"]);
    }

    fn reg(src: &str) -> std::collections::BTreeMap<String, Arc<ActorDef>> {
        let mut m = std::collections::BTreeMap::new();
        let d = def(src);
        m.insert(d.name.clone(), d);
        m
    }

    #[test]
    fn session_spawn_tell_ask() {
        let defs = reg("actor acc(state) { on add(n) { state = state + n; reply state } }");
        let decls = vec![RefDecl {
            name: "w".into(),
            actor: "acc".into(),
            init: 100.0,
            pool: None,
            restart: true,
        }];
        let mut s = Session::build(&defs, &decls).unwrap();
        s.tell("w", "add", vec![5.0]); // fire-and-forget: state 100 -> 105
        assert_eq!(s.ask("w", "add", vec![10.0]), Some(115.0));
        // Unknown ref is a no-op / None, never a panic.
        s.tell("ghost", "add", vec![1.0]);
        assert_eq!(s.ask("ghost", "add", vec![1.0]), None);
    }

    #[test]
    fn session_pool_round_robins() {
        // Each worker starts at its own base; two asks hit two distinct workers.
        let defs = reg("actor id(state) { on get { reply state } }");
        let decls = vec![RefDecl {
            name: "p".into(),
            actor: "id".into(),
            init: 7.0,
            pool: Some(3),
            restart: true,
        }];
        let mut s = Session::build(&defs, &decls).unwrap();
        assert_eq!(s.ask("p", "get", vec![]), Some(7.0));
        assert_eq!(s.ask("p", "get", vec![]), Some(7.0));
    }

    #[test]
    fn supervised_pool_respawns_but_failstop_does_not() {
        let defs = reg("actor w(state) { on job(x) { reply x + 1 } }");
        // A one-worker supervised pool: kill the worker, ask -> respawned answer.
        let mut sup = Session::build(
            &defs,
            &[RefDecl {
                name: "p".into(),
                actor: "w".into(),
                init: 0.0,
                pool: Some(1),
                restart: true,
            }],
        )
        .unwrap();
        kill_pool_worker(&mut sup, "p");
        assert_eq!(sup.ask("p", "job", vec![41.0]), Some(42.0));

        // A fail-stop pool (restart=false): a dead worker is not respawned.
        let mut fs = Session::build(
            &defs,
            &[RefDecl {
                name: "p".into(),
                actor: "w".into(),
                init: 0.0,
                pool: Some(1),
                restart: false,
            }],
        )
        .unwrap();
        kill_pool_worker(&mut fs, "p");
        assert_eq!(fs.ask("p", "job", vec![41.0]), None);
    }

    /// Replace a pool's single worker with a dead mailbox (dropped receiver).
    fn kill_pool_worker(s: &mut Session, name: &str) {
        if let Some(Target::Pool { workers, .. }) = s.refs.get_mut(name) {
            let (tx, rx) = mpsc::channel();
            drop(rx);
            workers[0] = ActorRef { tx };
        }
    }

    #[test]
    fn spec_build_parses_refs_actions_and_supervise() {
        use crate::spec::{self, BindAction};
        let src = "\
actor worker(state) { on job(x) { reply x * 2 } }\n\
spawn w = worker(3)\n\
pool p = worker * 4\n\
supervise p { on crash { stop } }\n\
bind C-t tell w job(5)\n\
bind C-a ask .out p job(.th)\n";
        let sp = spec::build(&parse(src).unwrap()).unwrap();
        assert_eq!(sp.actor_refs.len(), 2);
        let w = &sp.actor_refs[0];
        assert_eq!((w.name.as_str(), w.actor.as_str(), w.init, w.pool), ("w", "worker", 3.0, None));
        let p = &sp.actor_refs[1];
        assert_eq!((p.name.as_str(), p.pool, p.restart), ("p", Some(4), false)); // stop policy
        // The two binds carry the actor actions.
        let tell = sp.binds.iter().find_map(|b| match &b.action {
            BindAction::ActorTell { refname, call } => Some((refname.clone(), call.clone())),
            _ => None,
        });
        assert_eq!(tell, Some(("w".to_string(), "job(5)".to_string())));
        let ask = sp.binds.iter().find_map(|b| match &b.action {
            BindAction::ActorAsk { ctrl, refname, call } => {
                Some((ctrl.clone(), refname.clone(), call.clone()))
            }
            _ => None,
        });
        assert_eq!(
            ask,
            Some(("out".to_string(), "p".to_string(), "job(.th)".to_string()))
        );
    }

    #[test]
    fn parse_call_splits_args_at_depth_zero() {
        let (msg, args) = parse_call("job(5, (1 + 2) * 3)").unwrap();
        assert_eq!(msg, "job");
        assert_eq!(args.len(), 2);
        let inputs = HashMap::new();
        assert_eq!(eval_args(&args, &inputs), vec![5.0, 9.0]);
    }

    #[test]
    fn eval_args_resolves_controls() {
        let (_msg, args) = parse_call("job(.th * 2)").unwrap();
        let mut inputs = HashMap::new();
        inputs.insert("th".to_string(), "21".to_string());
        assert_eq!(eval_args(&args, &inputs), vec![42.0]);
    }

    #[test]
    fn pool_respawns_a_dead_worker() {
        let d = def("actor w(state) { on job(x) { reply x + 1 } }");
        let sys = Mutex::new(ActorSystem::new());
        let pool = sys.lock().unwrap().pool(d.clone(), 2, 0.0);
        // Kill one slot by replacing its ref with a dropped-receiver sender.
        {
            let (tx, rx) = mpsc::channel();
            drop(rx);
            pool.refs.lock().unwrap()[0] = ActorRef { tx };
        }
        // Two asks rotate across both slots; the dead one is respawned and answers.
        assert_eq!(pool.ask(&sys, "job", vec![1.0]), Some(2.0));
        assert_eq!(pool.ask(&sys, "job", vec![41.0]), Some(42.0));
        assert_eq!(pool.size(), 2);
    }
}
