//! The tree-walking interpreter.
//!
//! Runs the items a [`crate::passes`] regionization produced: functions
//! register first (declaration hoisting), then top-level statements run
//! in source order. Closures work through the environment chain on
//! [`value::Env`]; array methods dispatch with their receiver.
//!
//! Documented M1 divergences from JavaScript:
//! * `for (let i ...)` closures capture one shared loop variable —
//!   capture the value explicitly (`const j = i`) for per-iteration
//!   snapshots.
//! * No exceptions (`try`/`catch`): runtime errors abort the program.
//! * `t op= v` evaluates the target twice.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::*;
use crate::value::{Env, Func, SetError, Value};

/// A runtime failure.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpError {
    pub message: String,
}

fn bail(msg: impl Into<String>) -> InterpError {
    InterpError {
        message: msg.into(),
    }
}

/// Control flow through statement evaluation.
enum Ctl {
    /// Produced a value that statement context discards.
    Val,
    Return(Value),
    Break,
    Continue,
}

/// The interpreter. `console.log` writes to `out` so tests capture it
/// and the Node differential compares it byte for byte.
pub struct Interp<'o> {
    out: &'o mut dyn std::io::Write,
}

/// A fully parsed program: top-level items in source order.
pub enum Item {
    Fn(FnDef),
    Stmt(Stmt),
}

impl<'o> Interp<'o> {
    pub fn new(out: &'o mut dyn std::io::Write) -> Self {
        Self { out }
    }

    /// Runs a whole program.
    pub fn run(&mut self, items: &[Item]) -> Result<(), InterpError> {
        let global = Env::new(None);
        // The non-writable numeric globals JavaScript always provides.
        global.declare("NaN", Value::Num(f64::NAN), true);
        global.declare("Infinity", Value::Num(f64::INFINITY), true);
        for item in items {
            if let Item::Fn(def) = item {
                global.declare(
                    &def.name,
                    self.closure(
                        Some(def.name.clone()),
                        def.params.clone(),
                        &def.body,
                        &global,
                    ),
                    false,
                );
            }
        }
        for item in items {
            if let Item::Stmt(stmt) = item {
                self.exec_stmt(&global, stmt)?;
            }
        }
        Ok(())
    }

    fn closure(
        &self,
        name: Option<String>,
        params: Vec<String>,
        body: &[Stmt],
        env: &Rc<Env>,
    ) -> Value {
        Value::Func(Rc::new(Func::Closure {
            name,
            params,
            body: FnBody::Block(body.to_vec()),
            env: env.clone(),
        }))
    }

    // ---- statements ----

    fn exec_block(&mut self, env: &Rc<Env>, stmts: &[Stmt]) -> Result<Ctl, InterpError> {
        let block_env = Env::new(Some(env.clone()));
        self.hoist_fns(&block_env, stmts);
        for stmt in stmts {
            match self.exec_stmt(&block_env, stmt)? {
                Ctl::Val => {}
                ctl => return Ok(ctl),
            }
        }
        Ok(Ctl::Val)
    }

    /// Registers `function` declarations in this block before the
    /// statements run (block-level hoisting).
    fn hoist_fns(&self, env: &Rc<Env>, stmts: &[Stmt]) {
        for stmt in stmts {
            if let Stmt::FnDecl(name, params, body) = stmt {
                env.declare(
                    name,
                    self.closure(Some(name.clone()), params.clone(), body, env),
                    false,
                );
            }
        }
    }

    fn exec_stmt(&mut self, env: &Rc<Env>, stmt: &Stmt) -> Result<Ctl, InterpError> {
        match stmt {
            Stmt::Empty | Stmt::FnDecl(..) => Ok(Ctl::Val),
            Stmt::Let { is_const, decls } => {
                for (name, init) in decls {
                    let value = match init {
                        Some(e) => self.eval(env, e)?,
                        None => Value::Undefined,
                    };
                    env.declare(name, value, *is_const);
                }
                Ok(Ctl::Val)
            }
            Stmt::Expr(expr) => {
                self.eval(env, expr)?;
                Ok(Ctl::Val)
            }
            Stmt::If(cond, then, els) => {
                if self.eval(env, cond)?.is_truthy() {
                    self.exec_stmt(env, then)
                } else if let Some(els) = els {
                    self.exec_stmt(env, els)
                } else {
                    Ok(Ctl::Val)
                }
            }
            Stmt::While(cond, body) => {
                while self.eval(env, cond)?.is_truthy() {
                    match self.exec_stmt(env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                let loop_env = Env::new(Some(env.clone()));
                if let Some(init) = init {
                    self.exec_stmt(&loop_env, init)?;
                }
                loop {
                    if let Some(cond) = cond {
                        if !self.eval(&loop_env, cond)?.is_truthy() {
                            break;
                        }
                    }
                    match self.exec_stmt(&loop_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                    if let Some(step) = step {
                        self.eval(&loop_env, step)?;
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
            } => {
                let iter_value = self.eval(env, iterable)?;
                let items: Vec<Value> = match iter_value {
                    Value::Arr(arr) => arr.borrow().clone(),
                    Value::Str(s) => s
                        .chars()
                        .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                        .collect(),
                    other => return Err(bail(format!("{} is not iterable", other.to_display()))),
                };
                for item in items {
                    let iter_env = Env::new(Some(env.clone()));
                    iter_env.declare(name, item, *is_const);
                    match self.exec_stmt(&iter_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::Block(stmts) => self.exec_block(env, stmts),
            Stmt::Return(expr) => {
                let value = match expr {
                    Some(e) => self.eval(env, e)?,
                    None => Value::Undefined,
                };
                Ok(Ctl::Return(value))
            }
            Stmt::Break => Ok(Ctl::Break),
            Stmt::Continue => Ok(Ctl::Continue),
        }
    }

    // ---- expressions ----

    fn eval(&mut self, env: &Rc<Env>, expr: &Expr) -> Result<Value, InterpError> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(Value::Str(Rc::from(s.as_str()))),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => env
                .get(name)
                .ok_or_else(|| bail(format!("{name} is not defined"))),
            Expr::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(env, item)?);
                }
                Ok(Value::Arr(Rc::new(RefCell::new(out))))
            }
            Expr::Unary(op, e) => {
                let v = self.eval(env, e)?;
                Ok(match op {
                    UnaryOp::Not => Value::Bool(!v.is_truthy()),
                    UnaryOp::Neg => Value::Num(-to_num(&v)),
                    UnaryOp::Typeof => match v {
                        Value::Num(_) => Value::Str("number".into()),
                        Value::Str(_) => Value::Str("string".into()),
                        Value::Bool(_) => Value::Str("boolean".into()),
                        Value::Undefined => Value::Str("undefined".into()),
                        Value::Null | Value::Arr(_) => Value::Str("object".into()),
                        Value::Func(_) => Value::Str("function".into()),
                    },
                })
            }
            Expr::Binary(op, l, r) => {
                let lv = self.eval(env, l)?;
                let rv = self.eval(env, r)?;
                Ok(binary(*op, lv, rv))
            }
            Expr::Logical(op, l, r) => {
                let lv = self.eval(env, l)?;
                let truthy = lv.is_truthy();
                match op {
                    LogicalOp::And if truthy => self.eval(env, r),
                    LogicalOp::And => Ok(lv),
                    LogicalOp::Or if truthy => Ok(lv),
                    LogicalOp::Or => self.eval(env, r),
                }
            }
            Expr::Eq(op, l, r) => {
                let lv = self.eval(env, l)?;
                let rv = self.eval(env, r)?;
                Ok(Value::Bool(match op {
                    EqOp::Strict => lv.strict_eq(&rv),
                    EqOp::Loose => lv.loose_eq(&rv),
                }))
            }
            Expr::Ternary(cond, then, els) => {
                if self.eval(env, cond)?.is_truthy() {
                    self.eval(env, then)
                } else {
                    self.eval(env, els)
                }
            }
            Expr::Assign(target, value) => {
                let v = self.eval(env, value)?;
                self.assign(env, target, v.clone())?;
                Ok(v)
            }
            Expr::Index(obj, index) => {
                let o = self.eval(env, obj)?;
                let i = self.eval(env, index)?;
                Ok(index_get(&o, &i))
            }
            Expr::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                Ok(member_get(&o, prop))
            }
            Expr::Call(callee, args) => {
                // console.log(...) — the interpreter's output channel.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        let mut line = String::new();
                        for (i, a) in args.iter().enumerate() {
                            if i > 0 {
                                line.push(' ');
                            }
                            let v = self.eval(env, a)?;
                            line.push_str(&v.to_display());
                        }
                        writeln!(self.out, "{line}")
                            .map_err(|e| bail(format!("write failed: {e}")))?;
                        return Ok(Value::Undefined);
                    }
                    // Method call with a receiver: arr.push(x), arr.map(f)...
                    let recv = self.eval(env, obj_expr)?;
                    let mut argv = Vec::with_capacity(args.len());
                    for a in args {
                        argv.push(self.eval(env, a)?);
                    }
                    return self.call_method(&recv, prop, argv);
                }
                let f = self.eval(env, callee)?;
                let mut argv = Vec::with_capacity(args.len());
                for a in args {
                    argv.push(self.eval(env, a)?);
                }
                self.call_value(&f, argv)
            }
            Expr::Arrow(params, body) => Ok(Value::Func(Rc::new(Func::Closure {
                name: None,
                params: params.clone(),
                body: (**body).clone(),
                env: env.clone(),
            }))),
            Expr::Fn(name, params, body) => Ok(Value::Func(Rc::new(Func::Closure {
                name: name.clone(),
                params: params.clone(),
                body: FnBody::Block(body.clone()),
                env: env.clone(),
            }))),
            Expr::Update(op, prefix, target) => {
                let old = self.read_target(env, target)?;
                let new = Value::Num(match op {
                    UpdateOp::Inc => to_num(&old) + 1.0,
                    UpdateOp::Dec => to_num(&old) - 1.0,
                });
                self.assign(env, target, new.clone())?;
                Ok(if *prefix { new } else { old })
            }
        }
    }

    fn read_target(&mut self, env: &Rc<Env>, target: &Target) -> Result<Value, InterpError> {
        match target {
            Target::Ident(name) => env
                .get(name)
                .ok_or_else(|| bail(format!("{name} is not defined"))),
            Target::Index(obj, idx) => {
                let o = self.eval(env, obj)?;
                let i = self.eval(env, idx)?;
                Ok(index_get(&o, &i))
            }
            Target::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                Ok(member_get(&o, prop))
            }
        }
    }

    fn assign(&mut self, env: &Rc<Env>, target: &Target, value: Value) -> Result<(), InterpError> {
        match target {
            Target::Ident(name) => match env.set(name, value) {
                Ok(()) => Ok(()),
                Err(SetError::Const) => {
                    Err(bail(format!("assignment to constant variable `{name}`")))
                }
                Err(SetError::NotFound) => Err(bail(format!("{name} is not defined"))),
            },
            Target::Index(obj, idx) => {
                let o = self.eval(env, obj)?;
                let i = self.eval(env, idx)?;
                index_set(&o, &i, value);
                Ok(())
            }
            Target::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                member_set(&o, prop, value);
                Ok(())
            }
        }
    }

    fn call_method(
        &mut self,
        recv: &Value,
        prop: &str,
        argv: Vec<Value>,
    ) -> Result<Value, InterpError> {
        match (recv, prop) {
            (Value::Arr(arr), "push") => {
                let mut arr = arr.borrow_mut();
                arr.extend(argv);
                Ok(Value::Num(arr.len() as f64))
            }
            (Value::Arr(arr), "pop") => {
                let mut arr = arr.borrow_mut();
                Ok(arr.pop().unwrap_or(Value::Undefined))
            }
            (Value::Arr(arr), "map") => {
                let f = argv
                    .first()
                    .cloned()
                    .ok_or_else(|| bail("map expects a callback"))?;
                let snapshot = arr.borrow().clone();
                let mut out = Vec::with_capacity(snapshot.len());
                for (i, item) in snapshot.into_iter().enumerate() {
                    out.push(self.call_value(&f, vec![item, Value::Num(i as f64)])?);
                }
                Ok(Value::Arr(Rc::new(RefCell::new(out))))
            }
            (Value::Arr(arr), "filter") => {
                let f = argv
                    .first()
                    .cloned()
                    .ok_or_else(|| bail("filter expects a callback"))?;
                let snapshot = arr.borrow().clone();
                let mut out = Vec::new();
                for (i, item) in snapshot.into_iter().enumerate() {
                    let keep = self.call_value(&f, vec![item.clone(), Value::Num(i as f64)])?;
                    if keep.is_truthy() {
                        out.push(item);
                    }
                }
                Ok(Value::Arr(Rc::new(RefCell::new(out))))
            }
            (recv, _) => Err(bail(format!(
                "{}.{} is not a function",
                recv.to_display(),
                prop
            ))),
        }
    }

    /// Calls a function value: closures push an environment holding the
    /// parameters; blocks hoist their own function declarations.
    fn call_value(&mut self, f: &Value, argv: Vec<Value>) -> Result<Value, InterpError> {
        let func = match f {
            Value::Func(f) => f.clone(),
            other => return Err(bail(format!("{} is not a function", other.to_display()))),
        };
        match &*func {
            Func::Native { f, .. } => f(&argv).map_err(|message| InterpError { message }),
            Func::Closure {
                params,
                body,
                env: def_env,
                ..
            } => {
                let call_env = Env::new(Some(def_env.clone()));
                for (i, p) in params.iter().enumerate() {
                    call_env.declare(p, argv.get(i).cloned().unwrap_or(Value::Undefined), false);
                }
                match body {
                    FnBody::Expr(expr) => self.eval(&call_env, expr),
                    FnBody::Block(stmts) => {
                        self.hoist_fns(&call_env, stmts);
                        for stmt in stmts {
                            match self.exec_stmt(&call_env, stmt)? {
                                Ctl::Val => {}
                                Ctl::Return(v) => return Ok(v),
                                Ctl::Break => return Err(bail("illegal break")),
                                Ctl::Continue => return Err(bail("illegal continue")),
                            }
                        }
                        Ok(Value::Undefined)
                    }
                }
            }
        }
    }
}

fn to_num(v: &Value) -> f64 {
    match v.to_number_value() {
        Value::Num(n) => n,
        _ => f64::NAN,
    }
}

fn binary(op: BinOp, lv: Value, rv: Value) -> Value {
    match op {
        BinOp::Add => {
            if matches!(lv, Value::Str(_)) || matches!(rv, Value::Str(_)) {
                Value::Str(Rc::from(
                    format!("{}{}", lv.to_display(), rv.to_display()).as_str(),
                ))
            } else {
                Value::Num(to_num(&lv) + to_num(&rv))
            }
        }
        BinOp::Sub => Value::Num(to_num(&lv) - to_num(&rv)),
        BinOp::Mul => Value::Num(to_num(&lv) * to_num(&rv)),
        BinOp::Div => Value::Num(to_num(&lv) / to_num(&rv)),
        BinOp::Rem => Value::Num(to_num(&lv) % to_num(&rv)),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            if let (Value::Str(a), Value::Str(b)) = (&lv, &rv) {
                let ord = match op {
                    BinOp::Lt => a < b,
                    BinOp::Gt => a > b,
                    BinOp::Le => a <= b,
                    BinOp::Ge => a >= b,
                    _ => unreachable!(),
                };
                return Value::Bool(ord);
            }
            let (a, b) = (to_num(&lv), to_num(&rv));
            if a.is_nan() || b.is_nan() {
                return Value::Bool(false);
            }
            Value::Bool(match op {
                BinOp::Lt => a < b,
                BinOp::Gt => a > b,
                BinOp::Le => a <= b,
                _ => a >= b,
            })
        }
    }
}

fn index_get(obj: &Value, index: &Value) -> Value {
    match (obj, index) {
        (Value::Arr(arr), i) => match i {
            Value::Num(n) if n.fract() == 0.0 && *n >= 0.0 => arr
                .borrow()
                .get(*n as usize)
                .cloned()
                .unwrap_or(Value::Undefined),
            Value::Str(s) if s.as_ref() == "length" => Value::Num(arr.borrow().len() as f64),
            _ => Value::Undefined,
        },
        (Value::Str(s), Value::Num(n)) if n.fract() == 0.0 && *n >= 0.0 => s
            .chars()
            .nth(*n as usize)
            .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
            .unwrap_or(Value::Undefined),
        (Value::Str(s), Value::Str(prop)) if prop.as_ref() == "length" => {
            Value::Num(s.chars().count() as f64)
        }
        _ => Value::Undefined,
    }
}

fn index_set(obj: &Value, index: &Value, value: Value) {
    if let (Value::Arr(arr), Value::Num(n)) = (obj, index) {
        if n.fract() == 0.0 && *n >= 0.0 {
            let mut arr = arr.borrow_mut();
            let idx = *n as usize;
            if idx >= arr.len() {
                arr.resize(idx + 1, Value::Undefined);
            }
            arr[idx] = value;
        }
    }
}

fn member_get(obj: &Value, prop: &str) -> Value {
    index_get(obj, &Value::Str(Rc::from(prop)))
}

fn member_set(obj: &Value, prop: &str, value: Value) {
    if let (Value::Arr(arr), "length") = (obj, prop) {
        if let Value::Num(n) = value {
            if n >= 0.0 {
                arr.borrow_mut().truncate(n as usize);
            }
        }
    }
}
