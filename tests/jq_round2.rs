//! Round-2 parity pins for arb's jq engine.
//!
//! Every row is a divergence the differential fuzzer or a targeted probe found
//! against jq 1.8.2 and that was then fixed: a program, its input documents
//! (separated by ` ;; `) and the output jq 1.8.2 produced, line by line.
//!
//! Two tests read the same table:
//!
//! * `pinned_outputs_hold` needs no `jq` — it checks arb against the measured
//!   output, so the regression is caught on every machine, CI included.
//! * `pinned_outputs_match_live_jq` re-measures each row against the `jq` on
//!   PATH, which keeps the table honest. It skips (loudly) unless that `jq` is
//!   the 1.8 line the rows were measured against.
//!
//! Each program runs as `. | (PROGRAM)` and every output is compared with the
//! lines joined by `\n`, so a string with an embedded newline and two outputs
//! compare alike — the same rule the fuzzer uses.

mod common;

use common::{arb_run, jq_run, reference_ok};

/// `(program, input documents, expected output)`.
const PINS: &[(&str, &str, &str)] = &[
    // --- regular expressions: Oniguruma semantics and error text
    (
        r##"try [match("(?=a)a")|.offset] catch ."##,
        r##""ba""##,
        r##"[1]"##,
    ),
    (
        r##"try [match("(?<=b)a")|.offset] catch ."##,
        r##""ba""##,
        r##"[1]"##,
    ),
    (
        r##"try [match("(?<!b)a")|.offset] catch ."##,
        r##""ba""##,
        r##"[]"##,
    ),
    (
        r##"try [match("(a)\\1")|.string] catch ."##,
        r##""baab""##,
        r##"["aa"]"##,
    ),
    (
        r##"try [match("(?<x>a)\\k<x>")|.captures[0].name] catch ."##,
        r##""baab""##,
        r##"["x"]"##,
    ),
    (
        r##"try [match("(?>a+)")|.length] catch ."##,
        r##""baab""##,
        r##"[2]"##,
    ),
    (
        r##"try [match("a++")|.length] catch ."##,
        r##""baab""##,
        r##"[2]"##,
    ),
    (
        r##"try [match("\\h+")|.string] catch ."##,
        r##""xyz123abcg""##,
        r##"[]"##,
    ),
    (
        r##"try [match("a\\Kb")|.offset] catch ."##,
        r##""ab""##,
        r##"[1]"##,
    ),
    (
        r##"try [match("\\G.";"g")|.string] catch ."##,
        r##""abc""##,
        r##"["a","b","c"]"##,
    ),
    (
        r##"try [match("\\R")|.length] catch ."##,
        r##""\r\n""##,
        r##"[2]"##,
    ),
    (
        r##"try [match("[[:alpha:]]+";"g")|.string] catch ."##,
        r##""ab1é""##,
        r##"["ab","é"]"##,
    ),
    (
        r##"try [match("\\p{L}+";"g")|.string] catch ."##,
        r##""ab1é""##,
        r##"["ab","é"]"##,
    ),
    (
        r##"try [match("a{,2}";"g")|.string] catch ."##,
        r##""aaa""##,
        r##"[]"##,
    ),
    (
        r##"try [match("(?i:a)b";"g")|.string] catch ."##,
        r##""AbaB""##,
        r##"["Ab"]"##,
    ),
    (r##"try test("ß";"i") catch ."##, r##""SS""##, r##"true"##),
    (
        r##"try [match("a|ab";"gl")|.string] catch ."##,
        r##""abab""##,
        r##"["ab","ab"]"##,
    ),
    (
        r##"try [match("a*?";"gn")|[.offset,.length]] catch ."##,
        r##""baab""##,
        r##"[[1,1],[2,1]]"##,
    ),
    (
        r##"try [match("";"g")|.offset] catch ."##,
        r##""😀""##,
        r##"[0,1,1,1,1]"##,
    ),
    (
        r##"try [match("";"g")|.offset] catch ."##,
        r##""aé""##,
        r##"[0,1,2,2]"##,
    ),
    (
        r##"try [match("(?=u)";"g")|.offset] catch ."##,
        r##""qux""##,
        r##"[1]"##,
    ),
    (
        r##"try [match("(a)|(b)";"g")|.captures|map(.offset)] catch ."##,
        r##""ab""##,
        r##"[[0,-1],[-1,1]]"##,
    ),
    (
        r##"try [match("(?<n>a)(b)")|.captures] catch ."##,
        r##""ab""##,
        r##"[[{"offset":0,"length":1,"string":"a","name":"n"},{"offset":1,"length":1,"string":"b","name":null}]]"##,
    ),
    (
        r##"try [match("(?=(a))";"g")|.captures[0]] catch ."##,
        r##""aa""##,
        r##"[{"offset":0,"string":"","length":0,"name":null},{"offset":1,"string":"","length":0,"name":null}]"##,
    ),
    (
        r##"try capture("(?<x>a)(?<y>b)?") catch ."##,
        r##""a""##,
        r##"{"x":"a","y":null}"##,
    ),
    (
        r##"try [scan("(a)(b)?")] catch ."##,
        r##""ab a""##,
        r##"[["a","b"],["a",null]]"##,
    ),
    (
        r##"try test("(") catch ."##,
        r##""a""##,
        r##"Regex failure: end pattern with unmatched parenthesis"##,
    ),
    (
        r##"try test("a)") catch ."##,
        r##""a""##,
        r##"Regex failure: unmatched close parenthesis"##,
    ),
    (
        r##"try test("*") catch ."##,
        r##""a""##,
        r##"Regex failure: target of repeat operator is not specified"##,
    ),
    (
        r##"try test("[a") catch ."##,
        r##""a""##,
        r##"Regex failure: premature end of char-class"##,
    ),
    (
        r##"try test("\\") catch ."##,
        r##""a""##,
        r##"Regex failure: end pattern at escape"##,
    ),
    (
        r##"try test("[z-a]") catch ."##,
        r##""a""##,
        r##"Regex failure: empty range in char class"##,
    ),
    (
        r##"try test("\\k<x>") catch ."##,
        r##""a""##,
        r##"Regex failure: undefined name <x> reference"##,
    ),
    (
        r##"try test("(?<n") catch ."##,
        r##""a""##,
        r##"Regex failure: invalid group name <n>"##,
    ),
    (
        r##"try test("\\p{Foo}") catch ."##,
        r##""a""##,
        r##"Regex failure: invalid character property name {Foo}"##,
    ),
    (
        r##"try test("(?z)a") catch ."##,
        r##""a""##,
        r##"Regex failure: undefined group option"##,
    ),
    (
        r##"try test("[]") catch ."##,
        r##""a""##,
        r##"Regex failure: empty char-class"##,
    ),
    (
        r##"try test("a{2,1}") catch ."##,
        r##""a""##,
        r##"Regex failure: upper is smaller than lower in repeat range"##,
    ),
    (
        r##"try test("(?<>a)") catch ."##,
        r##""a""##,
        r##"Regex failure: group name is empty"##,
    ),
    (
        r##"try test("a";"gq") catch ."##,
        r##""a""##,
        r##"gq is not a valid modifier string"##,
    ),
    (
        r##"try test("a";1) catch ."##,
        r##""a""##,
        r##"number (1) is not a string"##,
    ),
    (
        r##"try test(1) catch ."##,
        r##""a""##,
        r##"number not a string or array"##,
    ),
    (
        r##"try test("a") catch ."##,
        r##"1"##,
        r##"number (1) cannot be matched, as it is not a string"##,
    ),
    (
        r##"try test([]) catch ."##,
        r##""a""##,
        r##"array not a string or array"##,
    ),
    (
        r##"try split("a";"q") catch ."##,
        r##""bab""##,
        r##"qg is not a valid modifier string"##,
    ),
    (
        r##"try [splits("a";"gq")] catch ."##,
        r##""bab""##,
        r##"gqg is not a valid modifier string"##,
    ),
    (
        r##"try [scan("a";"q")] catch ."##,
        r##""bab""##,
        r##"gq is not a valid modifier string"##,
    ),
    (
        r##"try gsub("a";"b";"q") catch ."##,
        r##""bab""##,
        r##"qg is not a valid modifier string"##,
    ),
    (
        r##"try sub("a";"b";"q") catch ."##,
        r##""bab""##,
        r##"q is not a valid modifier string"##,
    ),
    (
        r##"try gsub("a";"b";1) catch ."##,
        r##""bab""##,
        r##"number (1) and string ("g") cannot be added"##,
    ),
    (
        r##"try [splits("a";1)] catch ."##,
        r##""bab""##,
        r##"number (1) and string ("g") cannot be added"##,
    ),
    (r##"try gsub("";"-") catch ."##, r##""😀""##, r##"-😀----"##),
    (
        r##"try [gsub("a";"b","c")] catch ."##,
        r##""aa""##,
        r##"["bb","cc"]"##,
    ),
    (
        r##"try gsub("(?<x>.)";"\(.x)\(.x)") catch ."##,
        r##""ab""##,
        r##"aabb"##,
    ),
    (r##"try gsub("^";">") catch ."##, r##""abc""##, r##">abc"##),
    (
        r##"try gsub("a*";"x") catch ."##,
        r##""baac""##,
        r##"xbxxcx"##,
    ),
    (
        r##"try [splits("a+")] catch ."##,
        r##""baab""##,
        r##"["b","b"]"##,
    ),
    (
        r##"try split("";null) catch ."##,
        r##""abc""##,
        r##"["","a","b","c",""]"##,
    ),
    (
        r##"try split(", *";"g") catch ."##,
        r##""a, b,c""##,
        r##"["a","b","c"]"##,
    ),
    // --- generators, limits and loops
    (r##"[limit(0; 1, 2)]"##, r##"null"##, r##"[]"##),
    (
        r##"try [limit(-1; 1, 2)] catch ."##,
        r##"null"##,
        r##"limit doesn't support negative count"##,
    ),
    (r##"[first(range(10;0;-3))]"##, r##"null"##, r##"[10]"##),
    (
        r##"[range(0; 1; 0.3)]"##,
        r##"null"##,
        r##"[0,0.3,0.6,0.8999999999999999]"##,
    ),
    (
        r##"[range(1,2; 3,4)]"##,
        r##"null"##,
        r##"[1,2,1,2,3,2,2,3]"##,
    ),
    (
        r##"try [range("a")] catch ."##,
        r##"null"##,
        r##"Range bounds must be numeric"##,
    ),
    (
        r##"[limit(3; range(1e1000; nan))]"##,
        r##"null"##,
        r##"[1E+1000,1.7976931348623157e+308,1.7976931348623157e+308]"##,
    ),
    (
        r##"[limit(2; range(100000000000000000001; 100000000000000000009))]"##,
        r##"null"##,
        r##"[]"##,
    ),
    (
        r##"[limit(3; 1|repeat(.*2))]"##,
        r##"null"##,
        r##"[2,2,2]"##,
    ),
    (
        r##"[foreach (1,2,3) as $x (0,100; . + $x; [$x,.])]"##,
        r##"null"##,
        r##"[[1,1],[2,3],[3,6],[1,101],[2,103],[3,106]]"##,
    ),
    (
        r##"[reduce (1,2) as $x (0,10; . + $x)]"##,
        r##"null"##,
        r##"[3,13]"##,
    ),
    (
        r##"[label $f | range(10) | ., (select(. == 3) | break $f)]"##,
        r##"null"##,
        r##"[0,1,2,3]"##,
    ),
    (
        r##"[label $a | label $b | 1, break $a, 2]"##,
        r##"null"##,
        r##"[1]"##,
    ),
    (r##"try error(null) catch ."##, r##"null"##, r##"null"##),
    (
        r##"[.[] | try error(null) catch .]"##,
        r##"[1,2]"##,
        r##"[null,null]"##,
    ),
    (
        r##"try (try error(null) catch error) catch ."##,
        r##"null"##,
        r##"null"##,
    ),
    // --- reduce re-enters with `.` moved out
    (
        r##"try [reduce .[] as $x ((1,2); . + $x)] catch ."##,
        r##"[3.0]"##,
        r##"Cannot iterate over null (null)"##,
    ),
    (
        r##"[reduce .[]? as $x ((1,2); . + $x)]"##,
        r##"[3.0]"##,
        r##"[4,2]"##,
    ),
    (
        r##"[foreach .[] as $x ((1,2); . + $x)]"##,
        r##"[3.0]"##,
        r##"[4,5]"##,
    ),
    (
        r##"[reduce .[]? as $x ((. as [$a] ?// $a | $a); error("U"))]"##,
        r##"[3.0]"##,
        r##"[[3.0]]"##,
    ),
    (
        r##"try [reduce .[] as $x ((. as [$a] ?// $a | $a); error("U"))] catch ."##,
        r##"[3.0]"##,
        r##"Cannot iterate over null (null)"##,
    ),
    (
        r##"[reduce .[]? as $x ((. as [$a] ?// $a | $a); .+1)]"##,
        r##"[3.0]"##,
        r##"[4]"##,
    ),
    // --- ?// catches everything raised after it, `break` included
    (
        r##"[.[] as [$a] ?// $a | $a]"##,
        r##"[[1],2]"##,
        r##"[1,2]"##,
    ),
    (
        r##"try ((.[] as [$a] ?// $a | $a) | if type=="number" then error("n") else . end) catch ."##,
        r##"[[1]]"##,
        r##"[1]"##,
    ),
    (
        r##"[(.[] as [$a] ?// $a | $a) | if type=="number" then error("n") else . end]?"##,
        r##"[[1]]"##,
        r##"[[1]]"##,
    ),
    (
        r##"first(. as [$a] ?// $a | $a)"##,
        r##"[1]"##,
        r##"1
[1]"##,
    ),
    (
        r##"[limit(1; .[] as [$a] ?// $a | $a)]"##,
        r##"[[1],[2]]"##,
        r##"[1,[1]]"##,
    ),
    (
        r##"[. as {a:$x} ?// [$x] ?// $x | [$x]]"##,
        r##"[5]"##,
        r##"[[5]]"##,
    ),
    // --- updates: first output then break
    (r##".[] |= (2, error("x"))"##, r##"[1,2]"##, r##"[2,2]"##),
    (r##".a |= (1, error("x"))"##, r##"{"a":0}"##, r##"{"a":1}"##),
    (
        r##"(.a,.b) |= (. + 1, error("x"))"##,
        r##"{"a":0,"b":1}"##,
        r##"{"a":1,"b":2}"##,
    ),
    (r##".[] |= (empty, 5)"##, r##"[1,2]"##, r##"[5,5]"##),
    (r##".[] |= empty"##, r##"[1,2,3]"##, r##"[]"##),
    (
        r##"(.[] | select(. > 1)) |= empty"##,
        r##"[1,2,3]"##,
        r##"[1]"##,
    ),
    (
        r##"try (.[] |= (. as [$a] ?// $a | $a)) catch ."##,
        r##"[[1],2]"##,
        r##"Paths must be specified as an array"##,
    ),
    (
        r##"try (.a |= ((. as [$a] ?// $a | $a), 100)) catch ."##,
        r##"{"a":[1]}"##,
        r##"Paths must be specified as an array"##,
    ),
    (
        r##"try [.[] |= (. as [$a] ?// $a | $a | if type == "number" then 7 else . end)] catch ."##,
        r##"[[1],2]"##,
        r##"Paths must be specified as an array"##,
    ),
    // --- path expressions
    (
        r##"try path(walk(.)) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":1},[]]"##,
    ),
    (
        r##"try path(walk(.a?)) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element "a" of 1"##,
    ),
    (
        r##"try path(walk(select(type=="number"))) catch ."##,
        r##"{}"##,
        r##"Invalid path expression near attempt to access element 0 of [{},[]]"##,
    ),
    (
        r##"try path(map_values(.)) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":1},[]]"##,
    ),
    (
        r##"try path(map_values(.[]?)) catch ."##,
        r##"[[1]]"##,
        r##"Invalid path expression near attempt to iterate through [1]"##,
    ),
    (
        r##"try path(from_entries) catch ."##,
        r##"[{"key":"a","value":1}]"##,
        r##"Invalid path expression near attempt to iterate through [{"a":1}]"##,
    ),
    (
        r##"try path(from_entries) catch ."##,
        r##"[]"##,
        r##"Invalid path expression near attempt to iterate through []"##,
    ),
    (
        r##"try path(sub("a";"b")) catch ."##,
        r##""aa""##,
        r##"Invalid path expression near attempt to iterate through [{"offset":0,"length":1,"...]"##,
    ),
    (
        r##"try path(gsub("a";"b")) catch ."##,
        r##""aa""##,
        r##"Invalid path expression near attempt to iterate through [{"offset":0,"length":1,"...]"##,
    ),
    (
        r##"try [path(splits("a"))] catch ."##,
        r##""aba""##,
        r##"[[{"start":null,"end":0,"next":1}],[{"start":1,"end":2,"next":3}],[{"start":3,"end":null,"next":null}]]"##,
    ),
    (
        r##"try path(tostring | reverse) catch ."##,
        r##"[1,2]"##,
        r##"Invalid path expression near attempt to access element 4 of "[1,2]""##,
    ),
    (
        r##"try path(sort_by(length | .x?)) catch ."##,
        r##"[{"a":2},{"a":1}]"##,
        r##"Invalid path expression with result [{"a":2},{"a":1}]"##,
    ),
    (
        r##"try path(tojson | group_by(.)) catch ."##,
        r##"[1]"##,
        r##"Cannot iterate over string ("[1]")"##,
    ),
    (
        r##"try path(length) catch ."##,
        r##"5"##,
        r##"Invalid path expression with result 5"##,
    ),
    (
        r##"try path(floor) catch ."##,
        r##"5"##,
        r##"Invalid path expression with result 5"##,
    ),
    (
        r##"try path(.[0] | ceil) catch ."##,
        r##"[5]"##,
        r##"Invalid path expression with result 5"##,
    ),
    (
        r##"try path(.[0] | tojson | fromjson) catch ."##,
        r##"[5]"##,
        r##"Invalid path expression with result 5"##,
    ),
    (
        r##"try path(tostring | tonumber) catch ."##,
        r##"5"##,
        r##"Invalid path expression with result 5"##,
    ),
    (
        r##"try path(.[0] | tonumber) catch ."##,
        r##"[5]"##,
        r##"[0]"##,
    ),
    (r##"try path(.[0] | abs) catch ."##, r##"[5]"##, r##"[0]"##),
    (
        r##"try path(.[0] | length) catch ."##,
        r##"[1e1000]"##,
        r##"Invalid path expression with result 1E+1000"##,
    ),
    (r##"try path(nan) catch ."##, r##"nan"##, r##"[]"##),
    (
        r##"try path(.[0] | . + 0) catch ."##,
        r##"[nan]"##,
        r##"[0]"##,
    ),
    (r##"try path(debug) catch ."##, r##"5"##, r##"[]"##),
    (
        r##"try path(bsearch(1)) catch ."##,
        r##"[1,2]"##,
        r##"Invalid path expression with result 0"##,
    ),
    (
        r##"try path(. as [$a] | $a) catch ."##,
        r##"[[1],2]"##,
        r##"[0]"##,
    ),
    (
        r##"try path(. as [$a,$b] | $a) catch ."##,
        r##"[[1],2]"##,
        r##"Invalid path expression near attempt to access element 0 of [[1],2]"##,
    ),
    (
        r##"try path(. as [$a] | .) catch ."##,
        r##"[[1],2]"##,
        r##"Invalid path expression with result [[1],2]"##,
    ),
    (
        r##"try path(. as [$a] | .[1]) catch ."##,
        r##"[[1],2]"##,
        r##"Invalid path expression near attempt to access element 1 of [[1],2]"##,
    ),
    (
        r##"try path(.[0] as [$a] | $a) catch ."##,
        r##"[[1],2]"##,
        r##"Invalid path expression near attempt to access element 0 of [1]"##,
    ),
    (
        r##"try path(. as [[$a]] | $a) catch ."##,
        r##"[[1],2]"##,
        r##"[0,0]"##,
    ),
    (
        r##"try path(. as {a:$x} | .a) catch ."##,
        r##"null"##,
        r##"["a","a"]"##,
    ),
    (
        r##"try path(. as {a:$x} | .a) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element "a" of {"a":1}"##,
    ),
    (
        r##"try [path(. as [$a] ?// $a | .)] catch ."##,
        r##"[[1],2]"##,
        r##"[[]]"##,
    ),
    (
        r##"try path(.a and .b) catch ."##,
        r##"{"a":1,"b":2}"##,
        r##"Invalid path expression near attempt to access element "b" of {"a":1,"b":2}"##,
    ),
    (
        r##"try path(.a and .b) catch ."##,
        r##"{"a":false,"b":2}"##,
        r##"["a"]"##,
    ),
    (
        r##"try path(.a or .b) catch ."##,
        r##"{"a":false,"b":2}"##,
        r##"Invalid path expression near attempt to access element "b" of {"a":false,"b":2}"##,
    ),
    (
        r##"try path(.a and error("x")) catch ."##,
        r##"{"a":false}"##,
        r##"["a"]"##,
    ),
    (
        r##"try path(.a += .b) catch ."##,
        r##"{"a":1,"b":2}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":3,"b":2},[]]"##,
    ),
    (
        r##"try path(.a //= 1) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":1},[]]"##,
    ),
    (
        r##"try (((.[]? | tostring) //= 1) //= 5) catch ."##,
        r##"[1]"##,
        r##"Invalid path expression with result "1""##,
    ),
    (
        r##"try ((.a //= (map_values(.))) //= 5) catch ."##,
        r##"{"a":null}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":null},[]]"##,
    ),
    (
        r##"try path(.a |= 1) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":1},[]]"##,
    ),
    (
        r##"try path(setpath([{"start":1,"end":null}]; [])) catch ."##,
        r##"[3]"##,
        r##"[]"##,
    ),
    (
        r##"try path(setpath([{"start":0,"end":0}]; [1])) catch ."##,
        r##"[3]"##,
        r##"Invalid path expression with result [1,3]"##,
    ),
    (
        r##"try path(first(.a += 1)) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression near attempt to access element 0 of [{"a":2},[]]"##,
    ),
    (r##"pick(empty)"##, r##"[1]"##, r##"null"##),
    (r##"pick(first)"##, r##"[[],1]"##, r##"[[]]"##),
    (
        r##"try pick([.a]) catch ."##,
        r##"{"a":1}"##,
        r##"Invalid path expression with result [1]"##,
    ),
    (r##"pick(.[1:])"##, r##"[]"##, r##"[]"##),
    (
        r##"try pick(.a) catch ."##,
        r##"[1]"##,
        r##"Cannot index array with string ("a")"##,
    ),
    // --- delpaths / setpath / getpath
    (
        r##"try delpaths([[1],"a",["b"],{"x":1}]) catch ."##,
        r##"{"b":1}"##,
        r##"Path must be specified as array, not string"##,
    ),
    (
        r##"try delpaths([["a"],1]) catch ."##,
        r##"{"a":1}"##,
        r##"Path must be specified as array, not number"##,
    ),
    (
        r##"try delpaths(1) catch ."##,
        r##"{}"##,
        r##"Paths must be specified as an array"##,
    ),
    (
        r##"try setpath([{"b":1,"a":2}]; 1) catch ."##,
        r##"{"x":1}"##,
        r##"Cannot index object with object ({"b":1,"a":2})"##,
    ),
    (
        r##"try getpath([{"x":0}]) catch ."##,
        r##"[1,2,3]"##,
        r##"Array/string slice indices must be integers"##,
    ),
    (
        r##"try .[{"x":1}] catch ."##,
        r##"{"a":1}"##,
        r##"Cannot index object with object ({"x":1})"##,
    ),
    (
        r##"try .[1:2] catch ."##,
        r##"5"##,
        r##"Cannot index number with object ({"start":1,"end":2})"##,
    ),
    (
        r##"try getpath(["a","b"]) catch ."##,
        r##"{"a":[1]}"##,
        r##"Cannot index array with string ("b")"##,
    ),
    (
        r##"try getpath([null]) catch ."##,
        r##"{}"##,
        r##"Cannot index object with null (null)"##,
    ),
    (
        r##"try setpath([null]; 1) catch ."##,
        r##"null"##,
        r##"Cannot index null with null (null)"##,
    ),
    (r##"setpath([-1]; 9)"##, r##"[1,2]"##, r##"[1,9]"##),
    (
        r##"try setpath([-1]; 1) catch ."##,
        r##"[]"##,
        r##"Out of bounds negative array index"##,
    ),
    (
        r##"try setpath([1e10]; 1) catch ."##,
        r##"null"##,
        r##"Array index too large"##,
    ),
    // --- messages
    (
        r##"try (5|.["Cannot iterate over number (-1)"]) catch ."##,
        r##"null"##,
        r##"Cannot index number with string ("Cannot iterate over numb...")"##,
    ),
    (
        r##"try (5|.abcdefghijklmnopqrstuvwxyzabcdefghijkl) catch ."##,
        r##"null"##,
        r##"Cannot index number with string ("abcdefghijklmnopqrstuvwx...")"##,
    ),
    (
        r##"try ([1]|.[{"a":"0123456789012345678901234567890123456789"}]) catch ."##,
        r##"null"##,
        r##"Array/string slice indices must be integers"##,
    ),
    (
        r##"try frexp catch ."##,
        r##""a""##,
        r##"string ("a") number required"##,
    ),
    (
        r##"try modf catch ."##,
        r##"[1,2]"##,
        r##"array ([1,2]) number required"##,
    ),
    (
        r##"try lgamma_r catch ."##,
        r##"null"##,
        r##"null (null) number required"##,
    ),
    (
        r##"try format(1) catch ."##,
        r##"null"##,
        r##"number (1) is not a valid format"##,
    ),
    (
        r##"try format("foo") catch ."##,
        r##"null"##,
        r##"foo is not a valid format"##,
    ),
    (
        r##"try format(null) catch ."##,
        r##"null"##,
        r##"null (null) is not a valid format"##,
    ),
    (
        r##"try fromjson catch ."##,
        r##""\u0000""##,
        r##"Invalid numeric literal at EOF at line 1, column 1 (while parsing '')"##,
    ),
    (
        r##"try ("\u0000a"|fromjson) catch ."##,
        r##"null"##,
        r##"Invalid numeric literal at EOF at line 1, column 2 (while parsing '')"##,
    ),
    (r##"implode"##, r##"[-0.25]"##, r##" "##),
    (
        r##"implode"##,
        r##"[0.9, 1e10, 55296.5, 1114111.9]"##,
        r##" ��􏿿"##,
    ),
    (
        r##"[.[]|significand]"##,
        r##"[5e-324,1,8,-8,0.75,0,-0]"##,
        r##"[1,1,1,-1,1.5,0,-0]"##,
    ),
    (
        r##"[.[]|logb]"##,
        r##"[5e-324,1,8,-8,0.75,0,-0,0.1]"##,
        r##"[-1074,0,3,3,-1,-1.7976931348623157e+308,-1.7976931348623157e+308,-4]"##,
    ),
    // --- number formatting
    (
        r##"[.[]|tojson]"##,
        r##"[1.0,1.5,1e1000,-1e1000,100000000000000000000,100000000000000000001,1e-5,0.00001,1E2,3.0,-0,0.0,1.10]"##,
        r##"["1.0","1.5","1E+1000","-1E+1000","100000000000000000000","100000000000000000001","0.00001","0.00001","1E+2","3.0","-0","0.0","1.10"]"##,
    ),
    (
        r##"[.[]|.+0|tojson]"##,
        r##"[1.0,1e1000,100000000000000000000,1.5,0.1,3.0,-0]"##,
        r##"["1","1.7976931348623157e+308","1e+20","1.5","0.1","3","0"]"##,
    ),
    (
        r##"[nan,infinite,-infinite]|tojson"##,
        r##"null"##,
        r##"[null,1.7976931348623157e+308,-1.7976931348623157e+308]"##,
    ),
    (
        r##"[0|-.|-., (0.0|-.|-.), (-0|-.)]|tojson"##,
        r##"null"##,
        r##"[0,0.0,0]"##,
    ),
    (r##"reduce .[] as $x (0; -.)"##, r##"[1,2,3]"##, r##"0"##),
    (
        r##"[.[]|-(-.)]"##,
        r##"[0,-0,0.0,1,-1]"##,
        r##"[0,0,0.0,1,-1]"##,
    ),
    // --- formats
    (
        r##"@sh"##,
        r##"[1,"a'b",null,true]"##,
        r##"1 'a'\''b' null true"##,
    ),
    (
        r##"try @sh catch ."##,
        r##"{"a":1}"##,
        r##"object ({"a":1}) can not be escaped for shell"##,
    ),
    (
        r##"try @sh catch ."##,
        r##"[[1]]"##,
        r##"array ([1]) can not be escaped for shell"##,
    ),
    (
        r##"try @csv catch ."##,
        r##"[[1]]"##,
        r##"array ([1]) is not valid in a csv row"##,
    ),
    (
        r##"try @tsv catch ."##,
        r##"[{"a":1}]"##,
        r##"object ({"a":1}) is not valid in a csv row"##,
    ),
    (
        r##"@csv"##,
        r##"[1,"a\"b",null,true,1.5]"##,
        r##"1,"a""b",,true,1.5"##,
    ),
    (
        r##"@tsv"##,
        r##"[1,"a\tb\\c\nd",null,true,1.5]"##,
        r##"1	a\tb\\c\nd		true	1.5"##,
    ),
    (
        r##"@sh "echo \(.)""##,
        r##"["a","b c"]"##,
        r##"echo 'a' 'b c'"##,
    ),
    (
        r##"@uri"##,
        r##""a b&c=d/é~-_.!*'()""##,
        r##"a%20b%26c%3Dd%2F%C3%A9~-_.%21%2A%27%28%29"##,
    ),
    (
        r##"try @base64d catch ."##,
        r##""a""##,
        r##"string ("a") trailing base64 byte found"##,
    ),
    (r##"@base64d"##, r##""YQ""##, r##"a"##),
    (
        r##"@html"##,
        r##""<&>'\"""##,
        r##"&lt;&amp;&gt;&apos;&quot;"##,
    ),
    (
        r##"try @foo catch ."##,
        r##"null"##,
        r##"foo is not a valid format"##,
    ),
    // --- inputs
    (r##"[inputs]"##, r##"1 ;; 2 ;; 3"##, r##"[2,3]"##),
    (
        r##"[., (try input catch "E")]"##,
        r##"1 ;; 2 ;; 3"##,
        r##"[1,2]
[3,"E"]"##,
    ),
    (r##"try input catch ."##, r##"1"##, r##"break"##),
    (r##"first(inputs)"##, r##"1 ;; 2 ;; 3"##, r##"2"##),
    (
        r##"reduce inputs as $x (0; . + 1)"##,
        r##"1 ;; 2 ;; 3"##,
        r##"2"##,
    ),
    (
        r##"[input_line_number]"##,
        r##"1 ;; 2"##,
        r##"[1]
[2]"##,
    ),
    (
        r##"[., input_filename]"##,
        r##"1 ;; 2"##,
        r##"[1,"<stdin>"]
[2,"<stdin>"]"##,
    ),
    // --- sorting on several keys
    (
        r##"sort_by(.a, .b)"##,
        r##"[{"a":2,"b":1},{"a":1,"b":2},{"a":1,"b":1}]"##,
        r##"[{"a":1,"b":1},{"a":1,"b":2},{"a":2,"b":1}]"##,
    ),
    (
        r##"group_by(.a, .b)"##,
        r##"[{"a":2,"b":1},{"a":1,"b":2},{"a":1,"b":1},{"a":1,"b":1}]"##,
        r##"[[{"a":1,"b":1},{"a":1,"b":1}],[{"a":1,"b":2}],[{"a":2,"b":1}]]"##,
    ),
    (
        r##"unique_by(.a, .b)"##,
        r##"[{"a":2,"b":1},{"a":1,"b":2},{"a":1,"b":1},{"a":1,"b":1}]"##,
        r##"[{"a":1,"b":1},{"a":1,"b":2},{"a":2,"b":1}]"##,
    ),
    (
        r##"min_by(.a, .b)"##,
        r##"[{"a":2,"b":1},{"a":1,"b":2},{"a":1,"b":1}]"##,
        r##"{"a":1,"b":1}"##,
    ),
    (
        r##"max_by(.a, .b)"##,
        r##"[{"a":2,"b":1},{"a":1,"b":2},{"a":1,"b":1}]"##,
        r##"{"a":2,"b":1}"##,
    ),
    (
        r##"try _sort_by_impl(1) catch ."##,
        r##"[2]"##,
        r##"array ([2]) and number (1) cannot be sorted, as they are not both arrays"##,
    ),
    (
        r##"try _min_by_impl([[1]]) catch ."##,
        r##"{"a":1}"##,
        r##"object ({"a":1}) and array ([[1]]) cannot be iterated over"##,
    ),
    // --- strings
    (
        r##"try ltrimstr(1) catch ."##,
        r##""abc""##,
        r##"startswith() requires string inputs"##,
    ),
    (r##"trimstr("a")"##, r##""aba""##, r##"b"##),
    (
        r##"try implode catch ."##,
        r##"[[1]]"##,
        r##"array ([1]) can't be imploded, unicode codepoint needs to be numeric"##,
    ),
    (
        r##"try [.[]|@sh] catch ."##,
        r##"["a",1]"##,
        r##"["'a'","1"]"##,
    ),
];

fn docs(joined: &str) -> Vec<String> {
    joined.split(" ;; ").map(str::to_string).collect()
}

#[test]
fn pinned_outputs_hold() {
    let mut failures = Vec::new();
    for (program, input, expected) in PINS {
        let filter = format!(". | ({program})");
        match arb_run(&filter, &docs(input)) {
            Ok(lines) if lines.join("\n") == *expected => {}
            other => failures.push(format!(
                "{program}\n  input:    {input}\n  expected: {expected:?}\n  arb:      {other:?}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} pin(s) regressed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn pinned_outputs_match_live_jq() {
    if !reference_ok() {
        return;
    }
    let mut failures = Vec::new();
    for (program, input, expected) in PINS {
        let filter = format!(". | ({program})");
        match jq_run(&filter, &docs(input)) {
            Some(lines) if lines.join("\n") == *expected => {}
            other => failures.push(format!(
                "{program}\n  input:    {input}\n  pinned:   {expected:?}\n  jq:       {other:?}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} pin(s) no longer match jq:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `$__loc__` carries the line of its own token — counted through string
/// literals, comments and interpolations — which only a multi-line program can
/// show, so these run through the compiler directly rather than the one-line
/// pipeline DSL.
const LOC_PROGRAMS: &[(&str, &str)] = &[
    (
        "1 as $x\n| $__loc__",
        "{\"file\":\"<top-level>\",\"line\":2}",
    ),
    ("\n\n$__loc__.line", "3"),
    ("\"a\nb\" | $__loc__.line", "2"),
    ("# c\n$__loc__.line", "2"),
    ("\"\\n\\($__loc__.line)\"", "\n1"),
    ("\"a\\(\"x\"\n| $__loc__.line)\"", "a2"),
    ("[1,\n $__loc__.line,\n 2] | .[1]", "2"),
    ("def f: $__loc__.line;\n\nf", "1"),
    ("$__loc__.line, \n   $__loc__.line", "1\n2"),
    ("{a: $__loc__}\n| .a.file", "<top-level>"),
];

fn run_program(program: &str) -> String {
    use arb::jqlang::{render_raw, Interp, JqVal, Program};
    let compiled = Program::compile(program).expect("program compiles");
    let outputs = compiled
        .run(&Interp::default(), &JqVal::Null)
        .expect("program runs");
    outputs
        .iter()
        .map(render_raw)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn loc_is_the_line_of_its_token() {
    for (program, expected) in LOC_PROGRAMS {
        assert_eq!(run_program(program), *expected, "{program:?}");
    }
}

#[test]
fn loc_matches_live_jq() {
    if !reference_ok() {
        return;
    }
    for (program, _) in LOC_PROGRAMS {
        let out = std::process::Command::new("jq")
            .args(["-nrc", program])
            .output()
            .expect("run jq");
        let theirs = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            run_program(program),
            theirs.trim_end_matches('\n'),
            "{program:?}"
        );
    }
}
