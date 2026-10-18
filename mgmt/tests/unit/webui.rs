// The web UI's HTML escaping: every `${…}` in a template literal of app.js that builds HTML (innerHTML) must be
// escaped (esc), built by a helper that escapes, a literal, or a value known to be a number / fixed text / HTML built
// from escaped parts. CSP stops injected script anyway; this keeps a value from the server or the clients (hostnames,
// disk models, file names…) from breaking the page or injecting markup.
// ponytail: a small JS scanner (strings, regexes, comments, nested templates), not a JS parser — enough for app.js.

/// Template literals of a JS source: (their literal text, their `${…}` expressions), nested ones too.
fn templates(src: &str) -> Vec<(String, Vec<String>)> {
    struct Js<'a> {
        s: &'a [u8],
        i: usize,
        out: Vec<(String, Vec<String>)>,
    }
    impl Js<'_> {
        fn at(&self, k: usize) -> u8 {
            self.s.get(self.i + k).copied().unwrap_or(0)
        }
        /// Code until the end, or (inside `${`) until its closing brace.
        fn code(&mut self, in_expr: bool) {
            let (mut depth, mut prev) = (0usize, b'(');
            while self.i < self.s.len() {
                let c = self.at(0);
                match c {
                    b'\'' | b'"' => {
                        self.string(c);
                        prev = b'a';
                        continue;
                    }
                    b'`' => {
                        self.template();
                        prev = b'a';
                        continue;
                    }
                    b'/' if self.at(1) == b'/' => {
                        while self.i < self.s.len() && self.at(0) != b'\n' {
                            self.i += 1;
                        }
                        continue;
                    }
                    b'/' if self.at(1) == b'*' => {
                        self.i += 2;
                        while self.i < self.s.len() && !(self.at(0) == b'*' && self.at(1) == b'/') {
                            self.i += 1;
                        }
                        self.i += 2;
                        continue;
                    }
                    // A regex literal where a value starts (after an operator / bracket), else a division.
                    b'/' if b"(,=:[!&|?{};+-*%<>~^".contains(&prev) => {
                        self.regex();
                        prev = b'a';
                        continue;
                    }
                    b'{' => depth += 1,
                    b'}' if depth == 0 && in_expr => {
                        self.i += 1;
                        return;
                    }
                    b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                if !c.is_ascii_whitespace() {
                    prev = c;
                }
                self.i += 1;
            }
        }
        fn string(&mut self, q: u8) {
            self.i += 1;
            while self.i < self.s.len() {
                match self.at(0) {
                    b'\\' => self.i += 2,
                    x if x == q => {
                        self.i += 1;
                        return;
                    }
                    _ => self.i += 1,
                }
            }
        }
        fn regex(&mut self) {
            self.i += 1;
            let mut class = false;
            while self.i < self.s.len() {
                match self.at(0) {
                    b'\\' => self.i += 2,
                    b'[' => (class, self.i) = (true, self.i + 1),
                    b']' => (class, self.i) = (false, self.i + 1),
                    b'/' if !class => {
                        self.i += 1;
                        return;
                    }
                    b'\n' => return,
                    _ => self.i += 1,
                }
            }
        }
        fn template(&mut self) {
            self.i += 1;
            let (mut lit, mut exprs) = (String::new(), Vec::new());
            while self.i < self.s.len() {
                match self.at(0) {
                    b'\\' => {
                        lit.push(self.at(1) as char);
                        self.i += 2;
                    }
                    b'`' => {
                        self.i += 1;
                        break;
                    }
                    b'$' if self.at(1) == b'{' => {
                        self.i += 2;
                        let start = self.i;
                        self.code(true);
                        exprs
                            .push(String::from_utf8_lossy(&self.s[start..self.i - 1]).into_owned());
                        lit.push('\u{1}');
                    }
                    c => {
                        lit.push(c as char);
                        self.i += 1;
                    }
                }
            }
            self.out.push((lit, exprs));
        }
    }
    let mut js = Js {
        s: src.as_bytes(),
        i: 0,
        out: Vec::new(),
    };
    js.code(false);
    js.out
}

/// `e` split at its top-level `sep` (outside brackets, strings and templates).
fn split_top<'a>(e: &'a str, sep: &str) -> Vec<&'a str> {
    let b = e.as_bytes();
    let (mut parts, mut depth, mut start, mut i, mut q) = (Vec::new(), 0i32, 0, 0, 0u8);
    while i < b.len() {
        let c = b[i];
        if q != 0 {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                q = 0;
            }
        } else if matches!(c, b'\'' | b'"' | b'`') {
            q = c;
        } else if matches!(c, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(c, b')' | b']' | b'}') {
            depth -= 1;
        } else if depth == 0
            && e[i..].starts_with(sep)
            && !(sep == "?" && (e[i..].starts_with("?.") || e[i..].starts_with("??")))
        {
            parts.push(&e[start..i]);
            i += sep.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(&e[start..]);
    parts
}

/// Helpers that return escaped / fixed HTML or text made from numbers (each one read when added here).
const HTML_HELPERS: &[&str] = &[
    "act",
    "bBar",
    "baseBadge",
    "basePick",
    "cnt",
    "dur",
    "fld",
    "gbs",
    "grNames",
    "ic",
    "led",
    "licCell",
    "mb",
    "mxBadge",
    "opts",
    "osPill",
    "rate",
    "row",
    "sp",
    "String",
    "upBadge",
    "upFor",
    "volCell",
    "volFree",
    "CSS.escape",
    "Math.round",
    "Math.min",
    "Math.max",
];
/// Plain values known to be numbers or fixed text, or HTML built from escaped parts just before (read when added).
const SAFE_VALUES: &[&str] = &[
    // numbers from the server's typed JSON
    "d.id",
    "i.id",
    "m.id",
    "d.limit",
    "d.size_gb",
    "t.sessions",
    "s.layers",
    "s.old_sessions",
    "s.ver",
    "s.sessions",
    // inside helpers: their parameters are literals at every call site (ic's icon name, fld's label / id, row's
    // label), or HTML the caller built from escaped parts (fld's input)
    "n",
    "c??n",
    "ICON_LABEL[n]",
    "label",
    "id",
    "input",
    "name",
    // HTML just built from escaped parts in the same function: rows of a table, <option>s, badges
    "who",
    "opt",
    "rows",
    "drows",
    "vrows",
    "lease",
    "role",
    "bits.join(' · ')",
    "badge[d.state]",
    "opts.join('')",
];

/// `e` as cond ? a : b (top-level, nested ternaries in the branches kept whole).
fn ternary(e: &str) -> Option<(&str, &str)> {
    let q = split_top(e, "?");
    if q.len() < 2 {
        return None;
    }
    let rest = &e[q[0].len() + 1..];
    // The ':' matching the first '?': count the nested ones on the way.
    let (b, mut depth, mut nest, mut quote) = (rest.as_bytes(), 0i32, 0, 0u8);
    for (i, &c) in b.iter().enumerate() {
        if quote != 0 {
            if c == quote && b[i.saturating_sub(1)] != b'\\' {
                quote = 0;
            }
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = c,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'?' if depth == 0 && b.get(i + 1) != Some(&b'.') && b.get(i + 1) != Some(&b'?') => {
                nest += 1
            }
            b':' if depth == 0 && nest == 0 => return Some((&rest[..i], &rest[i + 1..])),
            b':' if depth == 0 => nest -= 1,
            _ => {}
        }
    }
    None
}

/// Does the `(` that starts `e` close only at its very end (`(a)+(b)` does not)?
fn closes_at_end(e: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in e.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return i == e.len() - 1;
        }
    }
    false
}

/// Is `${e}` safe in HTML?
fn escaped(e: &str) -> bool {
    let e = e.trim();
    let quoted = |q: char| {
        e.len() >= 2 && e.starts_with(q) && e.ends_with(q) && split_top(e, "+").len() == 1
    };
    if e.is_empty() || quoted('\'') || quoted('"') || quoted('`') {
        return true; // a literal (a nested template is checked on its own)
    }
    if e.parse::<f64>().is_ok() || SAFE_VALUES.contains(&e) {
        return true;
    }
    // ( … ) → what is inside.
    if e.starts_with('(') && closes_at_end(e) {
        return escaped(&e[1..e.len() - 1]);
    }
    // cond ? a : b → both branches.
    if let Some((a, b)) = ternary(e) {
        return escaped(a) && escaped(b);
    }
    // Numbers / text made of numbers: a count, a fixed-point number, a date.
    if e.ends_with(".length")
        || (e.contains(".toFixed(") && e.ends_with(')') && split_top(e, "+").len() == 1)
        || (e.starts_with("new Date(") && e.ends_with(".toLocaleString()"))
    {
        return true;
    }
    for sep in ["||", "??", "+"] {
        let p = split_top(e, sep);
        if p.len() > 1 {
            return p.iter().all(|x| escaped(x));
        }
    }
    // A call: esc(…), a helper, a list of templates joined.
    if let Some(open) = e.find('(')
        && e.ends_with(')')
        && (["esc", "encodeURIComponent"].contains(&&e[..open])
            || HTML_HELPERS.contains(&&e[..open]))
    {
        return true;
    }
    // A list of templates (checked on their own) or of escaped values, joined.
    let joined =
        e.ends_with(".join('')") || e.ends_with(".join(\"\")") || e.ends_with(".join('<br>')");
    joined && ((e.contains("=>") && e.contains('`')) || e.contains(".map(esc)"))
}

#[test]
fn ui_templates_escape_their_values() {
    let src = std::str::from_utf8(super::bytes("static/app.js")).unwrap();
    let mut bad = Vec::new();
    for (lit, exprs) in templates(src) {
        if !(lit.contains('<') || lit.contains("=\"")) {
            continue; // not HTML (a URL, a message, a CSS selector)
        }
        for e in exprs {
            if !escaped(&e) {
                bad.push(e);
            }
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{} unescaped values in HTML templates of app.js (esc() them, or add a helper / safe value to this test after \
         checking it):\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The scanner itself: strings, a regex with a backtick, comments and nested templates don't confuse it.
#[test]
fn scanner_sees_templates() {
    let t = templates(
        "const esc=s=>s.replace(/[`'\"]/g,'');// `no`\nx=`<b>${a?`<i>${esc(b)}</i>`:'-'}</b>`; y='`'; z=`${c}`;",
    );
    let exprs: Vec<_> = t.iter().flat_map(|(_, e)| e.clone()).collect();
    assert_eq!(exprs, ["esc(b)", "a?`<i>${esc(b)}</i>`:'-'", "c"]);
    assert!(
        escaped("a?`<i>${esc(b)}</i>`:'-'")
            && escaped("m.grp?esc(m.grp):'—'")
            && !escaped("m.hostname")
    );
    assert!(!escaped("x.name.toUpperCase()") && !escaped("a?b:'-'"));
}
