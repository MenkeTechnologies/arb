//! Token stream -> command tree. A command is the run of args between two
//! separators; the first arg is the verb. Block tokens are parsed recursively.
//! Each command records the char offset of its verb (for LSP diagnostics).

use crate::ast::{Arg, Command};
use crate::err::SpecError;
use crate::lexer::{lex, Tok};

/// Parse spec source into a command tree.
pub fn parse(src: &str) -> Result<Vec<Command>, SpecError> {
    parse_at(src, 0)
}

/// Parse `src`, treating its offsets as `base`-relative in the whole document
/// (so a nested `{ … }` block's commands still point at absolute source spans).
fn parse_at(src: &str, base: usize) -> Result<Vec<Command>, SpecError> {
    let toks = lex(src)?;
    let mut cmds = Vec::new();
    let mut cur: Vec<Arg> = Vec::new();
    let mut cur_pos: Option<usize> = None;
    for (t, off) in &toks {
        match t {
            Tok::Sep => {
                if !cur.is_empty() {
                    cmds.push(finish(std::mem::take(&mut cur), cur_pos.take().unwrap_or(base))?);
                }
            }
            Tok::Word(w) => {
                cur_pos.get_or_insert(base + off);
                cur.push(Arg::Word(w.clone()));
            }
            Tok::Str(s) => {
                cur_pos.get_or_insert(base + off);
                cur.push(Arg::Str(s.clone()));
            }
            Tok::Block(raw) => {
                cur_pos.get_or_insert(base + off);
                // The block's inner text starts one char after the `{`.
                cur.push(Arg::Block(parse_at(raw, base + off + 1)?));
            }
        }
    }
    if !cur.is_empty() {
        cmds.push(finish(cur, cur_pos.unwrap_or(base))?);
    }
    Ok(cmds)
}

fn finish(mut args: Vec<Arg>, pos: usize) -> Result<Command, SpecError> {
    let name = match args.first() {
        Some(Arg::Word(s)) | Some(Arg::Str(s)) => s.clone(),
        Some(Arg::Block(_)) => {
            return Err(SpecError { msg: "command cannot start with a block".into(), span: Some((pos, pos + 1)) })
        }
        None => return Err(SpecError { msg: "empty command".into(), span: Some((pos, pos + 1)) }),
    };
    args.remove(0);
    Ok(Command { name, args, pos })
}
