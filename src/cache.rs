//! rkyv-backed script cache for parsed `.arb` specs (mirrors the sibling langs
//! zshrs/rubylang/elisprs/vimlrs/awkrs). Versioned from day one so a spec that
//! parsed once never breaks on a later run.
//!
//! Layout: a single shard at `~/.arb/scripts.rkyv` (or `$ARB_CACHE`). The *outer*
//! container is a zero-copy rkyv archive (`Shard`), validated on load; each
//! *inner* entry blob is a bincode-encoded parsed AST (`Vec<ast::Command>`). arb
//! caches the AST rather than the built `Spec` because the `Spec` carries
//! `regex::Regex` (in `expect`/`match`/`replace`), which is neither serde- nor
//! rkyv-serializable — so a cache hit skips lex+parse and hands the AST straight
//! to `spec::build`, which recompiles the (cheap) regex/pipeline. The key is a
//! 64-bit hash of the source plus a schema version, so a source or format change
//! misses cleanly instead of loading a stale tree.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use rkyv::{Archive, Deserialize as RkyvDe, Serialize as RkyvSer};

use crate::ast::Command;

/// Bump on any incompatible change to `ast::Command`/`ast::Arg` or the parser
/// output shape, so old blobs miss instead of decoding wrong.
const SCHEMA: u64 = 1;

/// The outer, rkyv-archived shard: a flat list of (key, bincode-blob) entries.
#[derive(Archive, RkyvSer, RkyvDe, Default)]
#[archive(check_bytes)]
struct Shard {
    entries: Vec<Entry>,
}

#[derive(Archive, RkyvSer, RkyvDe)]
#[archive(check_bytes)]
struct Entry {
    key: u64,
    blob: Vec<u8>,
}

/// A stable content key for a source string (schema-salted).
pub fn key_for(src: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    SCHEMA.hash(&mut h);
    src.hash(&mut h);
    h.finish()
}

/// `~/.arb/scripts.rkyv` (or `$ARB_CACHE`), creating the parent dir. `None` when
/// there is no `$HOME`/`$ARB_CACHE` (the cache is then silently disabled).
fn shard_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ARB_CACHE") {
        let p = PathBuf::from(p);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return Some(p);
    }
    let dir = std::env::var_os("HOME").map(|h| Path::new(&h).join(".arb"))?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("scripts.rkyv"))
}

fn load_shard() -> Shard {
    let Some(path) = shard_path() else {
        return Shard::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Shard::default();
    };
    // A corrupt/old-format shard validates-fails and resets cleanly.
    rkyv::from_bytes::<Shard>(&bytes).unwrap_or_default()
}

fn write_shard(shard: &Shard) -> Result<(), String> {
    let path = shard_path().ok_or("no home dir for cache")?;
    let bytes = rkyv::to_bytes::<_, 4096>(shard).map_err(|e| format!("cache serialize: {e}"))?;
    std::fs::write(&path, &bytes).map_err(|e| format!("cache write: {e}"))
}

/// Encode a parsed AST to the inner bincode blob.
fn encode(ast: &[Command]) -> Result<Vec<u8>, String> {
    bincode::serialize(ast).map_err(|e| format!("cache encode: {e}"))
}

/// Decode an inner bincode blob back to a parsed AST.
fn decode(blob: &[u8]) -> Option<Vec<Command>> {
    bincode::deserialize(blob).ok()
}

/// The blob for `key` in `shard`, if present (pure; testable without the FS).
fn shard_get(shard: &Shard, key: u64) -> Option<&[u8]> {
    shard
        .entries
        .iter()
        .find(|e| e.key == key)
        .map(|e| e.blob.as_slice())
}

/// Insert/replace `key`'s blob in `shard` (pure; testable without the FS).
fn shard_put(shard: &mut Shard, key: u64, blob: Vec<u8>) {
    shard.entries.retain(|e| e.key != key);
    shard.entries.push(Entry { key, blob });
}

/// Look up the parsed AST for `src`, if present and current.
pub fn load(src: &str) -> Option<Vec<Command>> {
    let shard = load_shard();
    decode(shard_get(&shard, key_for(src))?)
}

/// Store the parsed `ast` for `src`, replacing any prior entry. Best-effort: any
/// error is returned but callers treat a cache miss/failure as non-fatal.
pub fn store(src: &str, ast: &[Command]) -> Result<(), String> {
    let blob = encode(ast)?;
    let mut shard = load_shard();
    shard_put(&mut shard, key_for(src), blob);
    write_shard(&shard)
}

/// Parse `src`, using the script cache: a hit skips lex+parse, a miss parses and
/// populates the cache (best-effort — a cache write failure never fails a run).
pub fn parse_cached(src: &str) -> Result<Vec<Command>, crate::err::SpecError> {
    if let Some(ast) = load(src) {
        return Ok(ast);
    }
    let ast = crate::parser::parse(src)?;
    let _ = store(src, &ast);
    Ok(ast)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Arg;

    #[test]
    fn key_is_stable_and_schema_salted() {
        assert_eq!(key_for("text .t <- in"), key_for("text .t <- in"));
        assert_ne!(key_for("text .t <- in"), key_for("tail .t <- in"));
    }

    #[test]
    fn encode_decode_round_trips_the_ast() {
        let ast = vec![Command {
            name: "gauge".into(),
            args: vec![
                Arg::Word(".cpu".into()),
                Arg::Block(vec![Command {
                    name: "in".into(),
                    args: vec![],
                    pos: 12,
                }]),
            ],
            pos: 0,
        }];
        let blob = encode(&ast).unwrap();
        let back = decode(&blob).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "gauge");
        // Nested block survives the round trip.
        match &back[0].args[1] {
            Arg::Block(inner) => assert_eq!(inner[0].name, "in"),
            _ => panic!("nested block lost"),
        }
    }

    #[test]
    fn shard_get_put_replaces_by_key() {
        let mut s = Shard::default();
        shard_put(&mut s, 7, vec![1, 2, 3]);
        shard_put(&mut s, 9, vec![4]);
        assert_eq!(shard_get(&s, 7), Some(&[1, 2, 3][..]));
        // Re-put the same key replaces, not appends.
        shard_put(&mut s, 7, vec![8, 8]);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(shard_get(&s, 7), Some(&[8, 8][..]));
        assert_eq!(shard_get(&s, 1), None);
    }

    #[test]
    fn parse_cached_matches_direct_parse_via_fs_round_trip() {
        // Isolate the shard to a scratch file so the test never touches ~/.arb.
        let tmp = std::env::temp_dir().join(format!("arb_cache_test_{}.rkyv", std::process::id()));
        std::env::set_var("ARB_CACHE", &tmp);
        let _ = std::fs::remove_file(&tmp);
        let src = "text .t <- in";
        // First call: cache miss -> parses + stores.
        let a = parse_cached(src).unwrap();
        // Second call: cache hit -> same AST as a direct parse.
        let b = parse_cached(src).unwrap();
        let direct = crate::parser::parse(src).unwrap();
        assert_eq!(a.len(), direct.len());
        assert_eq!(b.len(), direct.len());
        assert_eq!(b[0].name, direct[0].name);
        assert!(tmp.exists(), "shard should have been written");
        let _ = std::fs::remove_file(&tmp);
        std::env::remove_var("ARB_CACHE");
    }
}
