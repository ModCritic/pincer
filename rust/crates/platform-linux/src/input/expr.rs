//! The arithmetic language of `move_along_path`'s `parametric` x(t) and y(t)
//! and `parallel_mouse_drag`'s `fn` y(x): a function of one variable.
//!
//! It is the language the meval 0.2 crate accepted, kept when this parser
//! replaced that crate (last released in 2017, built on nom 1):
//!
//! ```text
//! expr    = term   (("+" | "-") term)*
//! term    = signed (("*" | "/" | "%") signed)*
//! signed  = ("+" | "-")* power
//! power   = primary ("^" signed)?
//! primary = number | name "(" expr ("," expr)* ")" | name | "(" expr ")"
//! number  = digit+ ("." digit*)? (("e" | "E") ("+" | "-")? digit+)?
//! name    = [A-Za-z_] [A-Za-z0-9_]*
//! ```
//!
//! Spaces, tabs, CR and LF may sit between tokens. `^` is right-associative
//! and binds tighter than a leading sign: `-2^2` is -4, `2^-2` is 0.25 and
//! `2^3^2` is 512. `%` is the floating-point remainder. A name is the bound
//! variable, the constant `pi` or `e`, or a function: sqrt, abs, exp, ln, sin,
//! cos, tan, asin, acos, atan, sinh, cosh, tanh, asinh, acosh, atanh, floor,
//! ceil, round and signum take one argument, atan2(y, x) two, max and min one
//! or more. The bound variable hides a constant of the same name.
//!
//! As in meval, every syntax error is reported before any name error, and
//! names are checked when the expression is compiled, so a compiled
//! expression always evaluates (to NaN or an infinity where the math does).
//!
//! Parsing emits reverse Polish notation that evaluation runs on a stack, so
//! a long flat chain such as `1+1+...+1` needs no recursion. Only nesting
//! (brackets, function arguments, the operand after `^`) recurses, and it is
//! capped at [`MAX_NESTING`] levels.

use std::f64::consts;

/// Deepest nesting of brackets, function calls and `^` operands accepted.
pub(crate) const MAX_NESTING: usize = 100;

/// Why an expression was refused. [`ExprError::is_syntax`] separates syntax
/// errors from unknown names and wrong argument counts.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum ExprError {
    #[error("unexpected `{found}` at byte {at}")]
    Unexpected { found: char, at: usize },
    #[error("malformed number at byte {at}")]
    BadNumber { at: usize },
    #[error("missing an operand at the end")]
    MissingOperand,
    #[error("missing {0} closing {}", if *.0 == 1 { "parenthesis" } else { "parentheses" })]
    MissingCloseParen(usize),
    #[error("nested more than {MAX_NESTING} levels deep")]
    TooDeep,
    #[error("unknown variable `{0}`")]
    UnknownVariable(String),
    #[error("unknown function `{0}`")]
    UnknownFunction(String),
    #[error("`{name}` takes {expected}, not {got}")]
    ArgumentCount {
        name: String,
        expected: &'static str,
        got: usize,
    },
}

impl ExprError {
    /// A syntax error, as opposed to an unknown name or a wrong argument
    /// count found once the syntax was valid.
    pub(crate) fn is_syntax(&self) -> bool {
        matches!(
            self,
            ExprError::Unexpected { .. }
                | ExprError::BadNumber { .. }
                | ExprError::MissingOperand
                | ExprError::MissingCloseParen(_)
                | ExprError::TooDeep
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

impl BinOp {
    fn apply(self, l: f64, r: f64) -> f64 {
        match self {
            BinOp::Add => l + r,
            BinOp::Sub => l - r,
            BinOp::Mul => l * r,
            BinOp::Div => l / r,
            BinOp::Rem => l % r,
            BinOp::Pow => l.powf(r),
        }
    }
}

/// Parser output: reverse Polish notation with names not yet resolved.
#[derive(Debug)]
enum Raw<'a> {
    Num(f64),
    Name(&'a str),
    Call(&'a str, usize),
    Neg,
    Bin(BinOp),
}

type Unary = fn(f64) -> f64;

#[derive(Clone, Copy, Debug)]
enum Op {
    Num(f64),
    Var,
    Neg,
    Bin(BinOp),
    F1(Unary),
    F2(fn(f64, f64) -> f64),
    Max(usize),
    Min(usize),
}

const FUNCTIONS_1: [(&str, Unary); 20] = [
    ("sqrt", f64::sqrt),
    ("exp", f64::exp),
    ("ln", f64::ln),
    ("abs", f64::abs),
    ("sin", f64::sin),
    ("cos", f64::cos),
    ("tan", f64::tan),
    ("asin", f64::asin),
    ("acos", f64::acos),
    ("atan", f64::atan),
    ("sinh", f64::sinh),
    ("cosh", f64::cosh),
    ("tanh", f64::tanh),
    ("asinh", f64::asinh),
    ("acosh", f64::acosh),
    ("atanh", f64::atanh),
    ("floor", f64::floor),
    ("ceil", f64::ceil),
    ("round", f64::round),
    ("signum", f64::signum),
];

/// An expression compiled against its one variable.
#[derive(Debug)]
pub(crate) struct Compiled {
    ops: Vec<Op>,
    stack: usize,
}

impl Compiled {
    /// The expression's value with the variable set to `v`.
    pub(crate) fn eval(&self, v: f64) -> f64 {
        let mut stack: Vec<f64> = Vec::with_capacity(self.stack);
        // The parser emits well-formed RPN (every operator finds its
        // operands), which the tests below check on every accepted input.
        let pop = |stack: &mut Vec<f64>| stack.pop().expect("well-formed RPN");
        for op in &self.ops {
            let value = match *op {
                Op::Num(n) => n,
                Op::Var => v,
                Op::Neg => -pop(&mut stack),
                Op::Bin(b) => {
                    let r = pop(&mut stack);
                    let l = pop(&mut stack);
                    b.apply(l, r)
                }
                Op::F1(f) => f(pop(&mut stack)),
                Op::F2(f) => {
                    let b = pop(&mut stack);
                    let a = pop(&mut stack);
                    f(a, b)
                }
                // Folds like meval's max_array/min_array: f64::max and
                // f64::min skip a NaN argument.
                Op::Max(n) => {
                    let args = stack.split_off(stack.len() - n);
                    args.iter().fold(f64::NEG_INFINITY, |m, &a| m.max(a))
                }
                Op::Min(n) => {
                    let args = stack.split_off(stack.len() - n);
                    args.iter().fold(f64::INFINITY, |m, &a| m.min(a))
                }
            };
            stack.push(value);
        }
        pop(&mut stack)
    }
}

/// Parse `src` and resolve its names with `var` as the one variable.
pub(crate) fn compile(src: &str, var: &str) -> Result<Compiled, ExprError> {
    let mut parser = Parser {
        src,
        pos: 0,
        depth: 0,
        open_parens: 0,
        out: Vec::new(),
    };
    parser.expr()?;
    parser.skip_space();
    if let Some(found) = parser.peek_char() {
        return Err(ExprError::Unexpected {
            found,
            at: parser.pos,
        });
    }
    resolve(parser.out, var)
}

fn resolve(raw: Vec<Raw<'_>>, var: &str) -> Result<Compiled, ExprError> {
    let mut ops = Vec::with_capacity(raw.len());
    let (mut depth, mut stack) = (0usize, 0usize);
    // Post-order, the order meval checked names in.
    for token in raw {
        let (op, pops) = match token {
            Raw::Num(n) => (Op::Num(n), 0),
            Raw::Name(name) if name == var => (Op::Var, 0),
            Raw::Name("pi") => (Op::Num(consts::PI), 0),
            Raw::Name("e") => (Op::Num(consts::E), 0),
            Raw::Name(name) => return Err(ExprError::UnknownVariable(name.to_owned())),
            Raw::Neg => (Op::Neg, 1),
            Raw::Bin(b) => (Op::Bin(b), 2),
            Raw::Call(name, n) => (function(name, n)?, n),
        };
        depth = depth + 1 - pops;
        stack = stack.max(depth);
        ops.push(op);
    }
    debug_assert_eq!(depth, 1, "the parser emits one value");
    Ok(Compiled { ops, stack })
}

fn function(name: &str, n: usize) -> Result<Op, ExprError> {
    let count = |expected: &'static str| ExprError::ArgumentCount {
        name: name.to_owned(),
        expected,
        got: n,
    };
    match name {
        "atan2" if n == 2 => Ok(Op::F2(f64::atan2)),
        "atan2" => Err(count("2 arguments")),
        "max" => Ok(Op::Max(n)),
        "min" => Ok(Op::Min(n)),
        _ => match FUNCTIONS_1.iter().find(|(known, _)| *known == name) {
            Some(&(_, f)) if n == 1 => Ok(Op::F1(f)),
            Some(_) => Err(count("1 argument")),
            None => Err(ExprError::UnknownFunction(name.to_owned())),
        },
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
    open_parens: usize,
    out: Vec<Raw<'a>>,
}

impl<'a> Parser<'a> {
    fn skip_space(&mut self) {
        let rest = &self.src.as_bytes()[self.pos..];
        self.pos += rest
            .iter()
            .take_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
    }

    /// The next character after any whitespace, not consumed.
    fn peek_char(&mut self) -> Option<char> {
        self.skip_space();
        self.src[self.pos..].chars().next()
    }

    /// Consume `want` if it is the next character after whitespace.
    fn eat(&mut self, want: u8) -> bool {
        self.skip_space();
        if self.src.as_bytes().get(self.pos) == Some(&want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn unexpected(&mut self) -> ExprError {
        match self.peek_char() {
            Some(found) => ExprError::Unexpected {
                found,
                at: self.pos,
            },
            None => ExprError::MissingOperand,
        }
    }

    fn expr(&mut self) -> Result<(), ExprError> {
        self.term()?;
        loop {
            let op = match self.peek_char() {
                Some('+') => BinOp::Add,
                Some('-') => BinOp::Sub,
                _ => return Ok(()),
            };
            self.pos += 1;
            self.term()?;
            self.out.push(Raw::Bin(op));
        }
    }

    fn term(&mut self) -> Result<(), ExprError> {
        self.signed()?;
        loop {
            let op = match self.peek_char() {
                Some('*') => BinOp::Mul,
                Some('/') => BinOp::Div,
                Some('%') => BinOp::Rem,
                _ => return Ok(()),
            };
            self.pos += 1;
            self.signed()?;
            self.out.push(Raw::Bin(op));
        }
    }

    /// Leading signs, then a power. Every recursion passes through here, so
    /// this is where nesting is counted.
    fn signed(&mut self) -> Result<(), ExprError> {
        if self.depth == MAX_NESTING {
            return Err(ExprError::TooDeep);
        }
        self.depth += 1;
        let mut negate = false;
        loop {
            match self.peek_char() {
                Some('+') => {}
                Some('-') => negate = !negate,
                _ => break,
            }
            self.pos += 1;
        }
        self.power()?;
        if negate {
            self.out.push(Raw::Neg);
        }
        self.depth -= 1;
        Ok(())
    }

    fn power(&mut self) -> Result<(), ExprError> {
        self.primary()?;
        if self.eat(b'^') {
            self.signed()?;
            self.out.push(Raw::Bin(BinOp::Pow));
        }
        Ok(())
    }

    fn primary(&mut self) -> Result<(), ExprError> {
        match self.peek_char() {
            Some('0'..='9') => self.number(),
            Some('a'..='z' | 'A'..='Z' | '_') => {
                let start = self.pos;
                let len = self.src.as_bytes()[start..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                    .count();
                self.pos += len;
                let name = &self.src[start..start + len];
                if self.eat(b'(') {
                    self.open_parens += 1;
                    let mut args = 1;
                    self.expr()?;
                    while self.eat(b',') {
                        self.expr()?;
                        args += 1;
                    }
                    self.close_paren()?;
                    self.out.push(Raw::Call(name, args));
                } else {
                    self.out.push(Raw::Name(name));
                }
                Ok(())
            }
            Some('(') => {
                self.pos += 1;
                self.open_parens += 1;
                self.expr()?;
                self.close_paren()
            }
            _ => Err(self.unexpected()),
        }
    }

    fn close_paren(&mut self) -> Result<(), ExprError> {
        if self.eat(b')') {
            self.open_parens -= 1;
            Ok(())
        } else if self.peek_char().is_none() {
            Err(ExprError::MissingCloseParen(self.open_parens))
        } else {
            Err(self.unexpected())
        }
    }

    fn number(&mut self) -> Result<(), ExprError> {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        let digits = |from: usize| bytes[from..].iter().take_while(|b| b.is_ascii_digit()).count();
        let mut end = start + digits(start);
        if bytes.get(end) == Some(&b'.') {
            end += 1 + digits(end + 1);
        }
        if matches!(bytes.get(end), Some(b'e' | b'E')) {
            // An exponent mark must start a whole exponent: `2e` and `2ex`
            // are malformed, as they were for meval.
            let mut at = end + 1;
            if matches!(bytes.get(at), Some(b'+' | b'-')) {
                at += 1;
            }
            let exponent = digits(at);
            if exponent == 0 {
                return Err(ExprError::BadNumber { at: start });
            }
            end = at + exponent;
        }
        let value = self.src[start..end]
            .parse::<f64>()
            .map_err(|_| ExprError::BadNumber { at: start })?;
        self.pos = end;
        self.out.push(Raw::Num(value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INF: f64 = f64::INFINITY;
    const NAN: f64 = f64::NAN;

    enum Want {
        /// Values at x = -2.5, 0.5 and 3.7 (or the named variable).
        V([f64; 3]),
        Syntax,
        Name,
    }
    use Want::*;

    /// Results of meval 0.2, the crate this parser replaced, recorded from it
    /// before it was removed. This parser then matched meval bit for bit on
    /// every accepted row at seven values of the variable, and on the error
    /// kind of every refused row, except the blank one meval panicked on.
    const MEVAL: &[(&str, &str, Want)] = &[
        ("x", "x", V([-2.5, 0.5, 3.7])),
        ("2*x+1", "x", V([-4.0, 2.0, 8.4])),
        ("100+50*sin(x)", "x", V([70.07639279480217, 123.97127693021015, 73.50819295457532])),
        ("-2^2", "x", V([-4.0, -4.0, -4.0])),
        ("2^-2", "x", V([0.25, 0.25, 0.25])),
        ("2^3^2", "x", V([512.0, 512.0, 512.0])),
        ("-x^2", "x", V([-6.25, -0.25, -13.690000000000001])),
        ("(-x)^2", "x", V([6.25, 0.25, 13.690000000000001])),
        ("2^-3^2", "x", V([0.001953125, 0.001953125, 0.001953125])),
        ("-2^2^2", "x", V([-16.0, -16.0, -16.0])),
        ("2*-3^2", "x", V([-18.0, -18.0, -18.0])),
        ("2^-3*4", "x", V([0.5, 0.5, 0.5])),
        ("2^-2+1", "x", V([1.25, 1.25, 1.25])),
        ("2 ^ - 3 ^ - 2", "x", V([0.9258747122872905, 0.9258747122872905, 0.9258747122872905])),
        ("-(2)^2", "x", V([-4.0, -4.0, -4.0])),
        ("-sin(x)^2", "x", V([-0.35816890726838696, -0.22984884706593015, -0.28072633621280485])),
        ("-2*3", "x", V([-6.0, -6.0, -6.0])),
        ("-2+3", "x", V([1.0, 1.0, 1.0])),
        ("2--3", "x", V([5.0, 5.0, 5.0])),
        ("--x", "x", V([-2.5, 0.5, 3.7])),
        ("+x", "x", V([-2.5, 0.5, 3.7])),
        ("+-+x", "x", V([2.5, -0.5, -3.7])),
        ("- x", "x", V([2.5, -0.5, -3.7])),
        ("-0", "x", V([-0.0, -0.0, -0.0])),
        ("x - -x", "x", V([-5.0, 1.0, 7.4])),
        ("1-2-3", "x", V([-4.0, -4.0, -4.0])),
        ("8/4/2", "x", V([1.0, 1.0, 1.0])),
        ("2*3%4", "x", V([2.0, 2.0, 2.0])),
        ("7%3", "x", V([1.0, 1.0, 1.0])),
        ("-7%3", "x", V([-1.0, -1.0, -1.0])),
        ("x%2", "x", V([-0.5, 0.5, 1.7000000000000002])),
        ("2+3*4^2", "x", V([50.0, 50.0, 50.0])),
        ("(2+3)*4", "x", V([20.0, 20.0, 20.0])),
        ("((x))", "x", V([-2.5, 0.5, 3.7])),
        ("sin(x)^2+cos(x)^2", "x", V([1.0, 1.0, 1.0])),
        ("sin (x)", "x", V([-0.5984721441039565, 0.479425538604203, -0.5298361409084934])),
        ("sqrt(x)", "x", V([NAN, 0.7071067811865476, 1.9235384061671346])),
        ("ln(x)", "x", V([NAN, -0.6931471805599453, 1.308332819650179])),
        ("exp(x)", "x", V([0.0820849986238988, 1.6487212707001282, 40.4473043600674])),
        ("abs(x)", "x", V([2.5, 0.5, 3.7])),
        ("tan(x)", "x", V([0.7470222972386603, 0.5463024898437905, 0.6247330752245639])),
        ("asin(x)", "x", V([NAN, 0.5235987755982989, NAN])),
        ("acos(x)", "x", V([NAN, 1.0471975511965979, NAN])),
        ("atan(x)", "x", V([-1.1902899496825317, 0.4636476090008061, 1.3068326031691921])),
        ("sinh(x)", "x", V([-6.0502044810397875, 0.5210953054937474, 20.21129041679853])),
        ("cosh(x)", "x", V([6.132289479663686, 1.1276259652063807, 20.23601394326887])),
        ("tanh(x)", "x", V([-0.9866142981514303, 0.46211715726000974, 0.9987782412811312])),
        ("asinh(x)", "x", V([-1.6472311463710958, 0.48121182505960347, 2.0192606561549606])),
        ("acosh(x)", "x", V([NAN, NAN, 1.9826969446812033])),
        ("atanh(x)", "x", V([NAN, 0.5493061443340548, NAN])),
        ("floor(x)", "x", V([-3.0, 0.0, 3.0])),
        ("ceil(x)", "x", V([-2.0, 1.0, 4.0])),
        ("round(x)", "x", V([-3.0, 1.0, 4.0])),
        ("signum(x)", "x", V([-1.0, 1.0, 1.0])),
        ("atan2(x, 1)", "x", V([-1.1902899496825317, 0.4636476090008061, 1.3068326031691921])),
        ("max(x)", "x", V([-2.5, 0.5, 3.7])),
        ("max(x, 1, -1)", "x", V([1.0, 1.0, 3.7])),
        ("min(x, 0)", "x", V([-2.5, 0.0, 0.0])),
        ("max(ln(x), 0)", "x", V([0.0, 0.0, 1.308332819650179])),
        ("min(sqrt(x), 9)", "x", V([9.0, 0.7071067811865476, 1.9235384061671346])),
        ("pi", "x", V([3.141592653589793, 3.141592653589793, 3.141592653589793])),
        ("e", "x", V([2.718281828459045, 2.718281828459045, 2.718281828459045])),
        ("e^x", "x", V([0.0820849986238988, 1.6487212707001282, 40.44730436006739])),
        ("2*pi*x", "x", V([-15.707963267948966, 3.141592653589793, 23.24778563656447])),
        ("e", "e", V([-2.5, 0.5, 3.7])),
        ("1e3", "x", V([1000.0, 1000.0, 1000.0])),
        ("1E-3*x", "x", V([-0.0025, 0.0005, 0.0037])),
        ("2.5e+2", "x", V([250.0, 250.0, 250.0])),
        ("2.", "x", V([2.0, 2.0, 2.0])),
        ("1.e2", "x", V([100.0, 100.0, 100.0])),
        ("0.125", "x", V([0.125, 0.125, 0.125])),
        ("  x  ", "x", V([-2.5, 0.5, 3.7])),
        ("\tx\n+\r1", "x", V([-1.5, 1.5, 4.7])),
        ("1/0", "x", V([INF, INF, INF])),
        ("0/0", "x", V([NAN, NAN, NAN])),
        ("x/0", "x", V([-INF, INF, INF])),
        ("1e400", "x", V([INF, INF, INF])),
        ("x^0.5", "x", V([NAN, 0.7071067811865476, 1.9235384061671346])),
        ("(-8)^(1/3)", "x", V([NAN, NAN, NAN])),
        ("1280+300*cos(t)", "t", V([1039.6569153359198, 1543.2747685671118, 1025.5699904868775])),
        ("500+10*t*sin(t)", "t", V([514.9618036025989, 502.397127693021, 480.39606278638576])),
        ("300+120*sin(x/40)", "x", V([292.50488185891436, 301.49996093780516, 311.0841777079914])),
        ("1+1+1+1+1+1+1+1+1+1", "x", V([10.0, 10.0, 10.0])),
        ("x_1", "x_1", V([-2.5, 0.5, 3.7])),
        ("", "x", Syntax),
        // meval 0.2 panicked on a blank expression.
        ("   ", "x", Syntax),
        ("x +", "x", Syntax),
        ("2^", "x", Syntax),
        ("-", "x", Syntax),
        ("()", "x", Syntax),
        ("2)", "x", Syntax),
        ("(((2)", "x", Syntax),
        ("f(2,)", "x", Syntax),
        ("f(,2)", "x", Syntax),
        ("max((1,2))", "x", Syntax),
        ("1,2", "x", Syntax),
        ("2 3", "x", Syntax),
        ("2x", "x", Syntax),
        ("x y", "x", Syntax),
        ("sin x", "x", Syntax),
        ("2e", "x", Syntax),
        ("2ex", "x", Syntax),
        ("1e+", "x", Syntax),
        (".5", "x", Syntax),
        ("1..2", "x", Syntax),
        ("1.5.2", "x", Syntax),
        ("x\u{b2}", "x", Syntax),
        ("2 ** 3", "x", Syntax),
        ("sin()", "x", Syntax),
        ("f(1)(2)", "x", Syntax),
        ("(", "x", Syntax),
        (")", "x", Syntax),
        ("x)", "x", Syntax),
        ("*2", "x", Syntax),
        ("-*2", "x", Syntax),
        ("sin(x", "x", Syntax),
        ("max(1,2", "x", Syntax),
        ("#", "x", Syntax),
        ("y +", "x", Syntax),
        ("foo(1", "x", Syntax),
        ("y", "x", Name),
        ("3*z", "x", Name),
        ("sin", "x", Name),
        ("foo(x)", "x", Name),
        ("x(2)", "x", Name),
        ("pi(2)", "x", Name),
        ("sin(1,2)", "x", Name),
        ("atan2(1)", "x", Name),
        ("atan2(1,2,3)", "x", Name),
        ("foo(y)", "x", Name),
        ("y + foo(1)", "x", Name),
        ("x*2", "t", Name),
        ("X", "x", Name),
        ("Pi", "x", Name),
        ("E", "x", Name),
    ];

    fn close(got: f64, want: f64) -> bool {
        got == want
            || (got.is_nan() && want.is_nan())
            || (got - want).abs() <= 1e-12 * want.abs()
    }

    #[test]
    fn matches_the_results_recorded_from_meval() {
        let mut wrong = Vec::new();
        for (src, var, want) in MEVAL {
            let got = compile(src, var);
            let ok = match (want, &got) {
                (V(values), Ok(f)) => [-2.5, 0.5, 3.7]
                    .iter()
                    .zip(values)
                    .all(|(&v, &w)| close(f.eval(v), w)),
                (Syntax, Err(e)) => e.is_syntax(),
                (Name, Err(e)) => !e.is_syntax(),
                _ => false,
            };
            if !ok {
                wrong.push(format!("{src:?} in {var}: {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn errors_name_the_problem() {
        let message = |src: &str| compile(src, "x").unwrap_err().to_string();
        assert_eq!(message("x +"), "missing an operand at the end");
        assert_eq!(message("(((2)"), "missing 2 closing parentheses");
        assert_eq!(message("sin(x"), "missing 1 closing parenthesis");
        assert_eq!(message("2 3"), "unexpected `3` at byte 2");
        assert_eq!(message("x\u{b2}"), "unexpected `\u{b2}` at byte 1");
        assert_eq!(message("2e"), "malformed number at byte 0");
        assert_eq!(message("y"), "unknown variable `y`");
        assert_eq!(message("foo(x)"), "unknown function `foo`");
        assert_eq!(message("sin(1,2)"), "`sin` takes 1 argument, not 2");
        assert_eq!(message("atan2(1)"), "`atan2` takes 2 arguments, not 1");
    }

    #[test]
    fn a_long_flat_chain_needs_no_recursion() {
        let src = vec!["1"; 10_000].join("+");
        assert_eq!(compile(&src, "x").unwrap().eval(0.0), 10_000.0);
    }

    #[test]
    fn nesting_is_capped() {
        let parens = |n: usize| format!("{}x{}", "(".repeat(n), ")".repeat(n));
        // Each level of brackets is one level; the top level is another.
        assert_eq!(compile(&parens(MAX_NESTING - 1), "x").unwrap().eval(2.0), 2.0);
        assert_eq!(compile(&parens(MAX_NESTING), "x").unwrap_err(), ExprError::TooDeep);
        let powers = |n: usize| vec!["1"; n].join("^");
        assert_eq!(compile(&powers(MAX_NESTING), "x").unwrap().eval(0.0), 1.0);
        assert_eq!(compile(&powers(MAX_NESTING + 1), "x").unwrap_err(), ExprError::TooDeep);
        // Signs are a loop, not nesting.
        let signs = format!("{}x", "-".repeat(1001));
        assert_eq!(compile(&signs, "x").unwrap().eval(2.0), -2.0);
    }
}
