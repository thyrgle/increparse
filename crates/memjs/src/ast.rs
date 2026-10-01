//! The memjs abstract syntax tree.
//!
//! Statements and expressions cover the M1 JavaScript subset: `let` /
//! `const`, assignment, `if` / `else`, `while`, `for`, `for..of`,
//! `break` / `continue` / `return`, function declarations and arrow
//! functions (closures), calls, member and index access, ternaries,
//! logical and arithmetic operators. Strings, f64 numbers, booleans,
//! `undefined`, `null`, and arrays.

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
    /// `typeof x`
    Typeof,
}

/// A binary arithmetic/comparison operator (`+ - * / % < > <= >=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A short-circuiting logical operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

/// An equality operator. `==` performs JavaScript coercion; `===` is strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqOp {
    Loose,
    Strict,
}

/// `++` / `--`, prefix or postfix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

/// The target shape of an assignment: `x`, `a[i]`, or `o.p`.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Ident(String),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    Array(Vec<Expr>),
    /// `{ a: 1, b }` — shorthand entries are resolved at parse time
    /// (`b` becomes `b: b`).
    Obj(Vec<ObjEntry>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Eq(EqOp, Box<Expr>, Box<Expr>),
    /// `target = value` — `target` is an [`Expr::Ident`], index, or member.
    Assign(Target, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `(params) => expr` or `(params) => { ... }`; a single unparenthesized
    /// parameter (`x => ...`) is normalized into the same shape.
    Arrow(Vec<String>, Box<FnBody>),
    /// A `function` expression, including named declarations.
    Fn(Option<String>, Vec<String>, Vec<Stmt>),
    Update(UpdateOp, bool, Target),
}

/// One `key: value` entry of an object literal. Keys are normalized to
/// strings (identifier, string, and numeric keys all become strings, as
/// JavaScript object keys are).
#[derive(Debug, Clone, PartialEq)]
pub struct ObjEntry {
    pub key: String,
    pub value: Expr,
}

/// The body of an arrow function: a bare expression or a block.
#[derive(Debug, Clone, PartialEq)]
pub enum FnBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `let x = e, y;` / `const x = e;` — one keyword, any number of
    /// declarators (JavaScript does not allow mixing keywords).
    Let {
        is_const: bool,
        decls: Vec<(String, Option<Expr>)>,
    },
    /// `var x = e, y;` — function-scoped and hoisted to the function
    /// frame (unlike `let`, which is block-scoped).
    Var {
        decls: Vec<(String, Option<Expr>)>,
    },
    Expr(Expr),
    If(Box<Expr>, Box<Stmt>, Option<Box<Stmt>>),
    While(Box<Expr>, Box<Stmt>),
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
    },
    /// `for (const x of iter) { ... }` / `for (let x of iter) { ... }`
    ForOf {
        name: String,
        is_const: bool,
        iterable: Expr,
        body: Box<Stmt>,
    },
    /// `for (const k in obj) { ... }` — iterates string keys (array
    /// indices as strings, per JavaScript).
    ForIn {
        name: String,
        is_const: bool,
        iterable: Expr,
        body: Box<Stmt>,
    },
    Block(Vec<Stmt>),
    Return(Option<Expr>),
    Break,
    Continue,
    /// A `function` declaration inside a body (hoisted within its block).
    FnDecl(String, Vec<String>, Vec<Stmt>),
    Empty,
}

/// A parsed top-level function.
#[derive(Debug, Clone, PartialEq)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}
