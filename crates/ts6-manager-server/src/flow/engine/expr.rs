//! v2 expression dialect — flow-engine redesign (PURA-266).
//!
//! `docs/flows/v2/architecture.md` §7 generalises v1.1's single-pass
//! `${trigger.key}` substitution into a deliberately small expression
//! language: path accessors over the run **blackboard**, comparisons,
//! boolean logic, and a handful of helpers. No arithmetic, no user-defined
//! functions, no loops — iteration is the `parallel` node (§2.2).
//!
//! §7.2 leaves the concrete library an implementation-time pick. We ship a
//! **self-contained evaluator** rather than pulling `minijinja` +
//! an `evalexpr`-class crate: the dialect is small enough that a built-in
//! recursive-descent parser is less surface than two new dependencies, and
//! it keeps the engine clean-room and WASM-neutral.
//!
//! Three entry points the engine uses:
//!   - [`eval`] — a bare expression → a JSON value (`transform` output,
//!     `parallel` collection).
//!   - [`eval_bool`] — a bare boolean expression (`branch` `when`).
//!   - [`interpolate`] — a `{{ … }}` template → a `String` (`action`
//!     argument templating, replacing v1.1 `${…}`).

use std::fmt;

use serde_json::{Map, Number, Value};

/// Failure evaluating or parsing an expression. The engine maps this onto a
/// node `errored` settle (`transform`) or a logged warning (`branch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError(pub String);

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ExprError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError(msg.into()))
}

// ---------------------------------------------------------------------------
// Blackboard (architecture.md §7.1)
// ---------------------------------------------------------------------------

/// The per-run data document an expression reads. Grows as nodes settle:
/// `trigger` is written once, `nodes.<id>` carries each settled node's
/// output, and `input` is the current node's single-inbound-edge data
/// (`Null` for a join with multiple inbound edges).
#[derive(Debug, Clone)]
pub struct Blackboard {
    root: Value,
}

impl Blackboard {
    /// Build a blackboard from the three §7.1 bindings.
    pub fn new(trigger: Value, nodes: Map<String, Value>, input: Option<Value>) -> Self {
        let mut root = Map::with_capacity(3);
        root.insert("trigger".to_string(), trigger);
        root.insert("nodes".to_string(), Value::Object(nodes));
        root.insert("input".to_string(), input.unwrap_or(Value::Null));
        Self {
            root: Value::Object(root),
        }
    }

    /// The trigger event document (`trigger` binding).
    pub fn trigger(&self) -> &Value {
        &self.root["trigger"]
    }

    /// The current node's inbound data (`input` binding).
    pub fn input(&self) -> &Value {
        &self.root["input"]
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Evaluate a bare expression against the blackboard.
pub fn eval(expr: &str, bb: &Blackboard) -> Result<Value, ExprError> {
    let ast = parse(expr)?;
    eval_ast(&ast, &bb.root)
}

/// Evaluate a bare boolean expression — the `branch` `when` form. Any value
/// is reduced to a boolean via [`truthy`].
pub fn eval_bool(expr: &str, bb: &Blackboard) -> Result<bool, ExprError> {
    Ok(truthy(&eval(expr, bb)?))
}

/// Render a `{{ … }}` template string against the blackboard. Text outside
/// `{{ }}` is copied verbatim; each `{{ expr }}` segment is evaluated and
/// stringified. This replaces v1.1's `${…}` substitution for `action`
/// argument templating (`architecture.md` §4.2 / §7.2).
pub fn interpolate(template: &str, bb: &Blackboard) -> Result<String, ExprError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return err("unterminated `{{ … }}` in template");
        };
        let value = eval(after[..end].trim(), bb)?;
        out.push_str(&stringify(&value));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// JSON truthiness: `false`/`null`/`0`/`""`/`[]`/`{}` are falsey.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Stringify a value for `{{ }}` interpolation: strings yield their raw
/// content (no JSON quoting), `null` yields the empty string, numbers and
/// booleans their literal form, and arrays/objects their JSON encoding.
fn stringify(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(f64),
    Str(String),
    True,
    False,
    Null,
    And,
    Or,
    Not,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
}

fn lex(src: &str) -> Result<Vec<Tok>, ExprError> {
    let mut toks = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                toks.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                toks.push(Tok::RBracket);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                toks.push(Tok::Dot);
                i += 1;
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Eq);
                    i += 2;
                } else {
                    return err("`=` is not an operator — did you mean `==`?");
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    return err("`!` is not an operator — use `not`");
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Le);
                    i += 2;
                } else {
                    toks.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            '"' | '\'' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return err("unterminated string literal");
                    };
                    if ch == '\\' {
                        match chars.get(i + 1) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('\\') => s.push('\\'),
                            Some(&q) if q == quote => s.push(q),
                            Some(&other) => s.push(other),
                            None => return err("unterminated escape in string literal"),
                        }
                        i += 2;
                        continue;
                    }
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                toks.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while chars.get(i).is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                }
                if chars.get(i) == Some(&'.')
                    && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                {
                    i += 1;
                    while chars.get(i).is_some_and(|d| d.is_ascii_digit()) {
                        i += 1;
                    }
                }
                let lit: String = chars[start..i].iter().collect();
                let n = lit
                    .parse::<f64>()
                    .map_err(|_| ExprError(format!("invalid number literal `{lit}`")))?;
                toks.push(Tok::Num(n));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|d| d.is_alphanumeric() || *d == '_')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                toks.push(match word.as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "null" => Tok::Null,
                    _ => Tok::Ident(word),
                });
            }
            other => return Err(ExprError(format!("unexpected character `{other}`"))),
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(Box<Ast>),
}

#[derive(Debug, Clone, PartialEq)]
enum Ast {
    Lit(Value),
    Path(String, Vec<Seg>),
    Call(String, Vec<Ast>),
    Not(Box<Ast>),
    Neg(Box<Ast>),
    Cmp(CmpOp, Box<Ast>, Box<Ast>),
    And(Box<Ast>, Box<Ast>),
    Or(Box<Ast>, Box<Ast>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

fn parse(src: &str) -> Result<Ast, ExprError> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return err("empty expression");
    }
    let mut p = Parser { toks, pos: 0 };
    let ast = p.parse_or()?;
    if p.pos != p.toks.len() {
        return Err(ExprError(format!(
            "trailing tokens after expression (at token {})",
            p.pos
        )));
    }
    Ok(ast)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok, ctx: &str) -> Result<(), ExprError> {
        match self.bump() {
            Some(ref got) if got == want => Ok(()),
            other => Err(ExprError(format!("expected {ctx}, found {other:?}"))),
        }
    }

    fn parse_or(&mut self) -> Result<Ast, ExprError> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = Ast::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Ast, ExprError> {
        let mut lhs = self.parse_not()?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            let rhs = self.parse_not()?;
            lhs = Ast::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Ast, ExprError> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Ast::Not(Box::new(self.parse_not()?)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Ast, ExprError> {
        let lhs = self.parse_primary()?;
        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.parse_primary()?;
        Ok(Ast::Cmp(op, Box::new(lhs), Box::new(rhs)))
    }

    fn parse_primary(&mut self) -> Result<Ast, ExprError> {
        match self.bump() {
            Some(Tok::Num(n)) => Ok(Ast::Lit(num_value(n))),
            Some(Tok::Str(s)) => Ok(Ast::Lit(Value::String(s))),
            Some(Tok::True) => Ok(Ast::Lit(Value::Bool(true))),
            Some(Tok::False) => Ok(Ast::Lit(Value::Bool(false))),
            Some(Tok::Null) => Ok(Ast::Lit(Value::Null)),
            Some(Tok::Lt) => err("unexpected `<`"),
            Some(Tok::LParen) => {
                let inner = self.parse_or()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(inner)
            }
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.parse_or()?);
                            match self.peek() {
                                Some(Tok::Comma) => self.pos += 1,
                                _ => break,
                            }
                        }
                    }
                    self.expect(&Tok::RParen, "`)` closing call")?;
                    Ok(Ast::Call(name, args))
                } else {
                    let segs = self.parse_path_segments()?;
                    Ok(Ast::Path(name, segs))
                }
            }
            other => Err(ExprError(format!(
                "unexpected token in expression: {other:?}"
            ))),
        }
    }

    fn parse_path_segments(&mut self) -> Result<Vec<Seg>, ExprError> {
        let mut segs = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    match self.bump() {
                        Some(Tok::Ident(k)) => segs.push(Seg::Key(k)),
                        other => {
                            return Err(ExprError(format!(
                                "expected field name after `.`, found {other:?}"
                            )));
                        }
                    }
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    let idx = self.parse_or()?;
                    self.expect(&Tok::RBracket, "`]`")?;
                    segs.push(Seg::Index(Box::new(idx)));
                }
                _ => break,
            }
        }
        Ok(segs)
    }
}

fn num_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.007_199_254_740_992e15 {
        Value::Number(Number::from(n as i64))
    } else {
        Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

fn eval_ast(ast: &Ast, root: &Value) -> Result<Value, ExprError> {
    match ast {
        Ast::Lit(v) => Ok(v.clone()),
        Ast::Path(head, segs) => eval_path(head, segs, root),
        Ast::Call(name, args) => eval_call(name, args, root),
        Ast::Not(inner) => Ok(Value::Bool(!truthy(&eval_ast(inner, root)?))),
        Ast::Neg(inner) => {
            let v = eval_ast(inner, root)?;
            let n = v
                .as_f64()
                .ok_or_else(|| ExprError("unary `-` needs a number".into()))?;
            Ok(num_value(-n))
        }
        Ast::And(a, b) => {
            if !truthy(&eval_ast(a, root)?) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(truthy(&eval_ast(b, root)?)))
        }
        Ast::Or(a, b) => {
            if truthy(&eval_ast(a, root)?) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(truthy(&eval_ast(b, root)?)))
        }
        Ast::Cmp(op, a, b) => {
            let lhs = eval_ast(a, root)?;
            let rhs = eval_ast(b, root)?;
            eval_cmp(op, &lhs, &rhs)
        }
    }
}

fn eval_path(head: &str, segs: &[Seg], root: &Value) -> Result<Value, ExprError> {
    let mut cur = root
        .get(head)
        .ok_or_else(|| ExprError(format!("unknown identifier `{head}`")))?
        .clone();
    for seg in segs {
        cur = match seg {
            Seg::Key(k) => cur
                .get(k)
                .ok_or_else(|| ExprError(format!("missing key `{k}`")))?
                .clone(),
            Seg::Index(idx_ast) => {
                let idx = eval_ast(idx_ast, root)?;
                match (&cur, &idx) {
                    (Value::Array(a), Value::Number(n)) => {
                        let i = n
                            .as_u64()
                            .ok_or_else(|| ExprError("array index must be a u64".into()))?;
                        a.get(i as usize)
                            .ok_or_else(|| ExprError(format!("array index {i} out of range")))?
                            .clone()
                    }
                    (Value::Object(_), Value::String(k)) => cur
                        .get(k)
                        .ok_or_else(|| ExprError(format!("missing key `{k}`")))?
                        .clone(),
                    _ => return err("`[…]` index needs array+number or object+string"),
                }
            }
        };
    }
    Ok(cur)
}

fn eval_call(name: &str, args: &[Ast], root: &Value) -> Result<Value, ExprError> {
    // `default(x, fallback)` evaluates `x` leniently: a missing-key error or
    // a `null` result falls back to the second argument.
    if name == "default" {
        if args.len() != 2 {
            return err("`default(x, fallback)` takes exactly 2 arguments");
        }
        return match eval_ast(&args[0], root) {
            Ok(Value::Null) | Err(_) => eval_ast(&args[1], root),
            Ok(v) => Ok(v),
        };
    }
    let vals: Vec<Value> = args
        .iter()
        .map(|a| eval_ast(a, root))
        .collect::<Result<_, _>>()?;
    match (name, vals.as_slice()) {
        ("len", [v]) => match v {
            Value::String(s) => Ok(num_value(s.chars().count() as f64)),
            Value::Array(a) => Ok(num_value(a.len() as f64)),
            Value::Object(o) => Ok(num_value(o.len() as f64)),
            _ => err("`len` needs a string, array, or object"),
        },
        ("lower", [Value::String(s)]) => Ok(Value::String(s.to_lowercase())),
        ("upper", [Value::String(s)]) => Ok(Value::String(s.to_uppercase())),
        ("lower" | "upper", _) => err("`lower`/`upper` need a single string argument"),
        ("contains", [Value::String(h), Value::String(n)]) => Ok(Value::Bool(h.contains(n))),
        ("contains", [Value::Array(a), needle]) => Ok(Value::Bool(a.contains(needle))),
        ("contains", _) => err("`contains` needs (string, string) or (array, value)"),
        ("len", _) => err("`len` takes exactly one argument"),
        (other, _) => Err(ExprError(format!("unknown function `{other}`"))),
    }
}

fn eval_cmp(op: &CmpOp, lhs: &Value, rhs: &Value) -> Result<Value, ExprError> {
    let result = match op {
        CmpOp::Eq => values_equal(lhs, rhs),
        CmpOp::Ne => !values_equal(lhs, rhs),
        _ => {
            let ord = compare(lhs, rhs)?;
            match op {
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                CmpOp::Le => ord != std::cmp::Ordering::Greater,
                CmpOp::Ge => ord != std::cmp::Ordering::Less,
                _ => unreachable!(),
            }
        }
    };
    Ok(Value::Bool(result))
}

/// Equality with number coercion: `1 == 1.0` is true even though the JSON
/// representations differ.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, ExprError> {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => {
            let x = a.as_f64().unwrap_or(f64::NAN);
            let y = b.as_f64().unwrap_or(f64::NAN);
            x.partial_cmp(&y)
                .ok_or_else(|| ExprError("cannot order NaN".into()))
        }
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        _ => err("`<`/`>`/`<=`/`>=` need two numbers or two strings"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bb() -> Blackboard {
        let mut nodes = Map::new();
        nodes.insert(
            "fetch_user".to_string(),
            json!({ "tier": "vip", "age": 30 }),
        );
        Blackboard::new(
            json!({ "channelId": 1, "clientNickname": "Alice", "newClients": [1, 2, 3] }),
            nodes,
            Some(json!({ "carried": true })),
        )
    }

    #[test]
    fn path_accessors_read_the_blackboard() {
        assert_eq!(eval("trigger.channelId", &bb()).unwrap(), json!(1));
        assert_eq!(eval("nodes.fetch_user.tier", &bb()).unwrap(), json!("vip"));
        assert_eq!(eval("input.carried", &bb()).unwrap(), json!(true));
        assert_eq!(eval("trigger.newClients[2]", &bb()).unwrap(), json!(3));
    }

    #[test]
    fn missing_key_is_a_strict_error() {
        let e = eval("trigger.nope", &bb()).unwrap_err();
        assert!(e.0.contains("missing key"), "got: {e}");
    }

    #[test]
    fn comparisons_and_boolean_logic() {
        assert!(eval_bool("trigger.channelId == 1", &bb()).unwrap());
        assert!(!eval_bool("trigger.channelId == 7", &bb()).unwrap());
        assert!(
            eval_bool(
                "trigger.channelId == 1 and nodes.fetch_user.tier == \"vip\"",
                &bb()
            )
            .unwrap()
        );
        assert!(eval_bool("nodes.fetch_user.age >= 18 or false", &bb()).unwrap());
        assert!(eval_bool("not (trigger.channelId == 7)", &bb()).unwrap());
    }

    #[test]
    fn number_equality_coerces() {
        assert!(eval_bool("trigger.channelId == 1.0", &bb()).unwrap());
    }

    #[test]
    fn builtins_cover_len_case_default_contains() {
        assert_eq!(eval("len(trigger.newClients)", &bb()).unwrap(), json!(3));
        assert_eq!(
            eval("lower(trigger.clientNickname)", &bb()).unwrap(),
            json!("alice")
        );
        assert_eq!(
            eval("default(trigger.nope, \"fallback\")", &bb()).unwrap(),
            json!("fallback")
        );
        assert_eq!(
            eval("default(trigger.channelId, 99)", &bb()).unwrap(),
            json!(1)
        );
        assert!(eval_bool("contains(trigger.newClients, 2)", &bb()).unwrap());
    }

    #[test]
    fn interpolation_replaces_segments() {
        assert_eq!(
            interpolate("Welcome {{ trigger.clientNickname }}.", &bb()).unwrap(),
            "Welcome Alice."
        );
        // A whole-string interpolation of a number yields the bare digits.
        assert_eq!(interpolate("{{ trigger.channelId }}", &bb()).unwrap(), "1");
        // No placeholders — copied verbatim.
        assert_eq!(interpolate("plain text", &bb()).unwrap(), "plain text");
    }

    #[test]
    fn interpolation_rejects_unterminated_template() {
        assert!(interpolate("{{ trigger.channelId", &bb()).is_err());
    }

    #[test]
    fn parse_errors_are_reported() {
        assert!(eval("trigger.channelId ==", &bb()).is_err());
        assert!(eval("", &bb()).is_err());
        assert!(eval("trigger.channelId 1", &bb()).is_err());
    }
}
