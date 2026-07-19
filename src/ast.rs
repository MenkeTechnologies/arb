//! Parsed command tree: a spec is a list of commands, each a verb plus args;
//! an arg may itself be a nested block of commands (e.g. a `source { … }` body).

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: Vec<Arg>,
    /// Char offset of the verb token in the whole source (for LSP diagnostics).
    pub pos: usize,
}

#[derive(Debug, Clone)]
pub enum Arg {
    Word(String),
    Str(String),
    Block(Vec<Command>),
}

impl Arg {
    /// The textual value of a word/string arg, or `None` for a block.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Arg::Word(s) | Arg::Str(s) => Some(s),
            Arg::Block(_) => None,
        }
    }
}
