//! YouTube stream-link unlocking: runs the site's own player script in an embedded
//! JavaScript engine and calls its signature and `n` functions, as the reference
//! download manager does with the Windows JScript engine.
//!
//! The player script is obfuscated (a string table plus multi-purpose functions that pick
//! their behaviour from numeric selector arguments), so the two functions are located
//! structurally and confirmed by behaviour instead of by fixed names:
//! - signature: every `name(num,num,…)` call in the script is tried on a test signature;
//!   the one that returns a rearranged subset of its input is the decipher function.
//! - n: the function whose `catch` returns `table[v^K] + input`, with `v` solved from its
//!   split-on-empty-string lookup and the selector found by behaviour.
//!
//! The result (function names, selectors, signature timestamp) is cached per player
//! version, so the search runs once each time the site ships a new player.

use boa_engine::{Context, JsString, JsValue, Source};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::mpsc;

/// How to call a flattened function: `name(f, w, value)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub name: String,
    pub f: i64,
    pub w: i64,
}

/// What was found in a player script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Solved {
    pub sts: u64,
    pub sig: Call,
    pub n: Call,
}

const PRELUDE: &str = r#"
var window = globalThis, self = globalThis;
var __noop = function () {};
var console = { log: __noop, warn: __noop, error: __noop, info: __noop, debug: __noop };
function setTimeout() { return 0 } function clearTimeout() {} function setInterval() { return 0 } function clearInterval() {}
var __el = function () { return { style: {}, setAttribute: __noop, appendChild: __noop, addEventListener: __noop, getContext: function () { return null }, classList: { add: __noop, remove: __noop } } };
var location = { href: "https://www.youtube.com/watch?v=x", hostname: "www.youtube.com", protocol: "https:", search: "", hash: "", pathname: "/watch", origin: "https://www.youtube.com" };
var navigator = { userAgent: "Mozilla/5.0", platform: "Win32", language: "en-US", languages: ["en-US"] };
var document = { createElement: __el, documentElement: __el(), head: __el(), body: __el(), getElementsByTagName: function () { return [] }, querySelector: function () { return null }, addEventListener: __noop, cookie: "" };
function XMLHttpRequest() {}
if (typeof Intl === "undefined") {
  var __fmt = function () { return { format: function (v) { return String(v) }, formatToParts: function () { return [] }, resolvedOptions: function () { return { locale: "en-US", timeZone: "UTC" } }, compare: function (a, b) { return a < b ? -1 : a > b ? 1 : 0 }, select: function () { return "other" }, segment: function (s) { return [] } } };
  var __ctor = function () { var f = function () { return __fmt() }; f.supportedLocalesOf = function (l) { return [].concat(l || []) }; return f };
  var Intl = { DateTimeFormat: __ctor(), NumberFormat: __ctor(), Collator: __ctor(), PluralRules: __ctor(), RelativeTimeFormat: __ctor(), ListFormat: __ctor(), Segmenter: __ctor(), DisplayNames: __ctor(), Locale: function (l) { this.baseName = l; this.language = String(l).split("-")[0] }, getCanonicalLocales: function (l) { return [].concat(l || []) }, supportedValuesOf: function () { return [] } };
}
"#;

const N_A: &str = "W5eLHGz9VbqmNAHlb";
const N_B: &str = "Xq1z9YbqmNAHlbW5e";
const SIG_TEST: &str = "AOq0QJ8wRAIgXYZabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-_abcdefgh=AiBCDEFGHIJKLMNOP";

/// `signatureTimestamp` of a player script.
pub fn timestamp(src: &str) -> Option<u64> {
    Regex::new(r"(?:signatureTimestamp|sts)\s*:\s*(\d{5,})").ok()?.captures(src)?.get(1)?.as_str().parse().ok()
}

/// The value of a JavaScript string literal's body.
fn unescape_js(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('x') => {
                let h: String = it.by_ref().take(2).collect();
                out.extend(u32::from_str_radix(&h, 16).ok().and_then(char::from_u32));
            }
            Some('u') => {
                let h: String = it.by_ref().take(4).collect();
                out.extend(u32::from_str_radix(&h, 16).ok().and_then(char::from_u32));
            }
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// The player's string table: (variable name, entries). It's a literal split on a one-
/// character separator (`;`, `}`, … varies by player version).
fn string_table(src: &str) -> Option<(String, Vec<String>)> {
    let re = Regex::new(r#"([A-Za-z_$][\w$]*)\s*=\s*(?:'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)")\.split\(\s*["'](.)["']\s*\)"#).ok()?;
    let mut best: Option<(String, Vec<String>)> = None;
    for c in re.captures_iter(src) {
        let Some(name) = c.get(1).map(|m| m.as_str()) else { continue };
        let Some(body) = c.get(2).or(c.get(3)).map(|m| unescape_js(m.as_str())) else { continue };
        let Some(sep) = c.get(4).map(|m| m.as_str()) else { continue };
        let entries: Vec<String> = body.split(sep).map(str::to_string).collect();
        if entries.iter().any(|e| e == "split") && entries.iter().any(|e| e == "join") && best.as_ref().is_none_or(|b| entries.len() > b.1.len()) {
            best = Some((name.to_string(), entries));
        }
    }
    best
}

/// Candidate signature calls: every `name(num,num,` in the script.
fn literal_calls(src: &str) -> Vec<Call> {
    let re = Regex::new(r"[^\w$.]([A-Za-z_$][\w$]{0,5})\((\d{1,4}),(\d{1,6}),").unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    const RESERVED: &[&str] = &["super", "if", "for", "while", "do", "return", "typeof", "new", "function", "switch", "catch", "with", "void", "delete", "in", "of", "case", "throw", "await", "yield", "import", "class", "this", "let", "var", "const"];
    for c in re.captures_iter(src) {
        if RESERVED.contains(&&c[1]) {
            continue;
        }
        let call = Call { name: c[1].to_string(), f: c[2].parse().unwrap_or(0), w: c[3].parse().unwrap_or(0) };
        if seen.insert((call.name.clone(), call.f, call.w)) {
            out.push(call);
        }
    }
    out
}

/// The n function: its name and the selector candidates (v values) to try.
fn n_function(src: &str, table: &(String, Vec<String>)) -> Option<(String, Vec<i64>)> {
    let t = regex::escape(&table.0);
    // catch(e){h = TABLE[v^K] + a; break ...}
    let marker = Regex::new(&format!(r"catch\(([\w$]+)\)\{{([\w$]+)={t}\[([\w$]+)\^(\d+)\]\+([\w$]+);break")).ok()?;
    let m = marker.captures(src)?;
    let pos = m.get(0)?.start();
    let k_marker: i64 = m[4].parse().ok()?;
    // Enclosing function: the last `name=function(a,b,…){var v=b^a;` before the marker.
    let head = Regex::new(r"([A-Za-z_$][\w$]*)=function\(([\w$]+),([\w$]+)[^)]*\)\{var ([\w$]+)=([\w$]+)\^([\w$]+);").ok()?;
    let start = pos.saturating_sub(200_000);
    let func = head.captures_iter(&src[start..pos]).last()?;
    let name = func[1].to_string();
    let fstart = start + func.get(0)?.start();
    let body = &src[fstart..pos];
    let len = table.1.len() as i64;
    // The catch returns TABLE[v^K] + input, and that entry is the error marker ("…_w8_").
    let mut vs: Vec<i64> = table.1.iter().enumerate().filter(|(_, e)| e.ends_with("_w8_")).map(|(i, _)| k_marker ^ i as i64).collect();
    if vs.is_empty() {
        // Older layout: the input is split on TABLE[v^K], which must be the empty string.
        if let Some(split_idx) = table.1.iter().position(|e| e == "split") {
            let split_re = Regex::new(&format!(r"\[{t}\[{split_idx}\]\]\({t}\[[\w$]+\^(\d+)\]\)")).ok()?;
            if let Some(k_sep) = split_re.captures_iter(body).last().and_then(|c| c[1].parse::<i64>().ok()) {
                vs = table.1.iter().enumerate().filter(|(_, e)| e.is_empty()).map(|(i, _)| k_sep ^ i as i64).filter(|v| (v ^ k_marker) < len).collect();
            }
        }
    }
    if vs.is_empty() {
        vs = (0..len).map(|i| k_marker ^ i).collect();
    }
    Some((name, vs))
}

struct Engine {
    ctx: Context,
}

impl Engine {
    /// Run the player script with the given functions exposed on `_yt_player.__rdm`.
    fn load(src: &str, expose: &[String]) -> Result<Self, String> {
        let end = src.rfind("})(_yt_player);").ok_or("unexpected player script layout")?;
        let list = expose.iter().map(|n| format!("{n:?}:(function(){{try{{return {n}}}catch(e){{}}}})()")).collect::<Vec<_>>().join(",");
        let code = format!("{PRELUDE}\n{}\n;_yt_player.__rdm={{{list}}};\n{}", &src[..end], &src[end..]);
        let mut ctx = Context::default();
        ctx.runtime_limits_mut().set_loop_iteration_limit(50_000_000);
        ctx.runtime_limits_mut().set_recursion_limit(2_000);
        ctx.eval(Source::from_bytes(code.as_bytes())).map_err(|e| format!("player script failed: {e}"))?;
        Ok(Self { ctx })
    }

    fn call(&mut self, call: &Call, value: &str) -> Option<String> {
        let player = self.ctx.global_object().get(JsString::from("_yt_player"), &mut self.ctx).ok()?;
        let exposed = player.as_object()?.get(JsString::from("__rdm"), &mut self.ctx).ok()?;
        let f = exposed.as_object()?.get(JsString::from(call.name.as_str()), &mut self.ctx).ok()?;
        let f = f.as_callable()?;
        let args = [JsValue::from(call.f as i32), JsValue::from(call.w as i32), JsValue::from(JsString::from(value))];
        let r = f.call(&JsValue::undefined(), &args, &mut self.ctx).ok()?;
        r.as_string().map(|s| s.to_std_string_escaped())
    }

    fn looks_like_n(&mut self, call: &Call) -> bool {
        let ok = |s: &str| (8..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let Some(a) = self.call(call, N_A) else { return false };
        if a == N_A || !ok(&a) || a.contains(N_A) {
            return false;
        }
        let Some(b) = self.call(call, N_B) else { return false };
        b != a && ok(&b) && self.call(call, N_A).as_deref() == Some(a.as_str())
    }

    fn looks_like_sig(&mut self, call: &Call) -> bool {
        let Some(s) = self.call(call, SIG_TEST) else { return false };
        s != SIG_TEST
            && s.len() + 12 >= SIG_TEST.len()
            && s.len() <= SIG_TEST.len()
            && s.chars().all(|c| SIG_TEST.contains(c))
            && self.call(call, SIG_TEST).as_deref() == Some(s.as_str())
    }
}

/// Find the signature and n functions in a player script.
fn discover(src: &str) -> Result<(Engine, Solved), String> {
    let sts = timestamp(src).ok_or("no signature timestamp in player script")?;
    let table = string_table(src).ok_or("no string table in player script")?;
    let (n_name, vs) = n_function(src, &table).ok_or("n function not found in player script")?;
    let calls = literal_calls(src);
    let mut names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
    names.push(n_name.clone());
    names.sort();
    names.dedup();
    let mut engine = Engine::load(src, &names)?;
    let sig = calls.iter().find(|c| engine.looks_like_sig(c)).cloned().ok_or("signature function not found")?;
    let mut n = None;
    'outer: for v in &vs {
        for f in 0..256i64 {
            let c = Call { name: n_name.clone(), f, w: v ^ f };
            if engine.looks_like_n(&c) {
                n = Some(c);
                break 'outer;
            }
        }
    }
    let n = n.ok_or("n function selector not found")?;
    Ok((engine, Solved { sts, sig, n }))
}

enum Msg {
    Sig(String, mpsc::Sender<Option<String>>),
    N(String, mpsc::Sender<Option<String>>),
}

/// A loaded player script on its own thread (the JS engine isn't thread-safe).
#[derive(Clone)]
pub struct Solver {
    tx: mpsc::Sender<Msg>,
    pub solved: Solved,
}

impl Solver {
    /// Load `src`, using `known` (from the cache) when it still checks out, else searching.
    pub fn open(src: String, known: Option<Solved>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Solved, String>>();
        std::thread::Builder::new()
            .name("yt-solver".into())
            .stack_size(64 << 20)
            .spawn(move || {
                let setup = (|| -> Result<(Engine, Solved), String> {
                    if let Some(k) = known {
                        let mut e = Engine::load(&src, &[k.sig.name.clone(), k.n.name.clone()])?;
                        if e.looks_like_sig(&k.sig) && e.looks_like_n(&k.n) {
                            return Ok((e, k));
                        }
                    }
                    discover(&src)
                })();
                let mut engine = match setup {
                    Ok((e, s)) => {
                        let _ = ready_tx.send(Ok(s.clone()));
                        (e, s)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(m) = rx.recv() {
                    match m {
                        Msg::Sig(s, reply) => {
                            let c = engine.1.sig.clone();
                            let _ = reply.send(engine.0.call(&c, &s));
                        }
                        Msg::N(s, reply) => {
                            let c = engine.1.n.clone();
                            let _ = reply.send(engine.0.call(&c, &s));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let solved = ready_rx.recv().map_err(|_| "player script solver stopped".to_string())??;
        Ok(Self { tx, solved })
    }

    pub fn sig(&self, s: &str) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::Sig(s.to_string(), tx)).ok()?;
        rx.recv().ok().flatten()
    }

    pub fn n(&self, n: &str) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Msg::N(n.to_string(), tx)).ok()?;
        rx.recv().ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against a real player script when RDM_YT_PLAYER points at one.
    #[test]
    fn solves_a_real_player_script() {
        let Ok(path) = std::env::var("RDM_YT_PLAYER") else { return };
        let src = std::fs::read_to_string(path).unwrap();
        let t0 = std::time::Instant::now();
        let s = Solver::open(src.clone(), None).expect("solve");
        eprintln!("found {:?} in {:?}", s.solved, t0.elapsed());
        let n = s.n("W5eLHGz9VbqmNAHlb").unwrap();
        let sig = s.sig(SIG_TEST).unwrap();
        eprintln!("n -> {n}, sig -> {}", &sig[..30]);
        if let Ok(expect) = std::env::var("RDM_YT_EXPECT_N") {
            assert_eq!(n, expect);
        }
        // Reopening with the cached result skips the search.
        let t1 = std::time::Instant::now();
        let again = Solver::open(src, Some(s.solved.clone())).unwrap();
        assert_eq!(again.n("W5eLHGz9VbqmNAHlb").unwrap(), n);
        eprintln!("cached open in {:?}", t1.elapsed());
    }
}
