//! `arb::run_program`: the headless `arb -e SRC` path with in-memory stdin and
//! stdout.

use arb::run_program;

/// A spec with no `out` and no stream source prints its widget summary — the
/// binary's no-terminal output — to the captured stdout.
#[test]
fn summary_is_captured() {
    let out = run_program("text .t", "").unwrap();
    assert!(out.starts_with("arb: spec — 1 widget(s)"), "{out:?}");
    assert!(out.ends_with("stream: 0 lines\n"), "{out:?}");
}

/// `input` is the stream an `out { … }` pipeline reshapes, both on the
/// per-line streaming path and on the whole-stream (reducer / jq `inputs`)
/// path.
#[test]
fn input_is_the_stream() {
    assert_eq!(
        run_program("out { in.json; .a + 1 }", "{\"a\":1}\n{\"a\":41}\n").unwrap(),
        "2\n42\n"
    );
    assert_eq!(
        run_program("out { in.json; [., inputs] | add }", "1\n2\n3\n").unwrap(),
        "6\n"
    );
}

/// A source widget with no `out` passes its input through unchanged.
#[test]
fn passthrough_copies_input() {
    let input = "alpha\nbeta\n";
    assert_eq!(
        run_program("tail .s\nsource .s { in }", input).unwrap(),
        input
    );
}

/// A spec that does not parse, and a jq error raised mid-stream, are `Err`
/// carrying arb's diagnostic — not a process exit.
#[test]
fn errors_are_err() {
    assert!(run_program("out {", "").is_err());
    let e = run_program("out { in.json; error(\"bad\") }", "1\n").unwrap_err();
    assert!(e.contains("bad"), "{e:?}");
}

/// jq `halt` is a successful exit: the output so far comes back as `Ok`.
#[test]
fn halt_is_ok() {
    assert_eq!(
        run_program("out { in.json; if . == 2 then halt else . end }", "1\n2\n3\n").unwrap(),
        "1\n"
    );
}

/// Concurrent runs each see their own input and return their own output.
#[test]
fn concurrent_runs_do_not_mix() {
    let runs: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let input: String = (0..2000).map(|n| format!("{}\n", n * 8 + i)).collect();
                let got = run_program("out { in.json; . * 10 }", &input).unwrap();
                let want: String = (0..2000).map(|n| format!("{}\n", (n * 8 + i) * 10)).collect();
                assert_eq!(got, want, "run {i}");
            })
        })
        .collect();
    for r in runs {
        r.join().unwrap();
    }
}
