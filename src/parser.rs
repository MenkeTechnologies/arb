//! Token stream -> command tree. A command is the run of args between two
//! separators; the first arg is the verb. Block tokens are parsed recursively.

use crate::ast::{Arg, Command};
use crate::lexer::{lex, Tok};

/// Parse spec source into a command tree.
pub fn parse(src: &str) -> Result<Vec<Command>, String> {
    let toks = lex(src)?;
    parse_tokens(&toks)
}

fn parse_tokens(toks: &[Tok]) -> Result<Vec<Command>, String> {
    let mut cmds = Vec::new();
    let mut cur: Vec<Arg> = Vec::new();
    for t in toks {
        match t {
            Tok::Sep => {
                if !cur.is_empty() {
                    cmds.push(finish(std::mem::take(&mut cur))?);
                }
            }
            Tok::Word(w) => cur.push(Arg::Word(w.clone())),
            Tok::Str(s) => cur.push(Arg::Str(s.clone())),
            Tok::Block(raw) => cur.push(Arg::Block(parse(raw)?)),
        }
    }
    if !cur.is_empty() {
        cmds.push(finish(cur)?);
    }
    Ok(cmds)
}

fn finish(mut args: Vec<Arg>) -> Result<Command, String> {
    let name = match args.first() {
        Some(Arg::Word(s)) | Some(Arg::Str(s)) => s.clone(),
        Some(Arg::Block(_)) => return Err("command cannot start with a block".into()),
        None => return Err("empty command".into()),
    };
    args.remove(0);
    Ok(Command { name, args })
}
