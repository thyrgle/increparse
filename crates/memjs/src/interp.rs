//! The tree-walking interpreter.
//!
//! Runs the items a [`crate::passes`] regionization produced: functions
//! register first (declaration hoisting), then top-level statements run
//! in source order. Closures work through the environment chain on
//! [`value::Env`]; array methods dispatch with their receiver.
//!
//! Documented divergences from JavaScript:
//! * No exceptions (`try`/`catch`): runtime errors abort the program.
//! * `t op= v` evaluates the target twice.
//! * Object literals support `key: value` and shorthand `key`, but not
//!   method shorthand, computed keys, or getters; there is no `delete`.
//! * `in` exists only in `for..in`, not as an operator.
//! * `console.log` inspects containers like Node, but prints very deep
//!   structures fully instead of truncating to `[Array]` / `[Object]`.

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
                hoist_vars(&global, stmt);
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
            Stmt::Var { decls } => {
                for (name, init) in decls {
                    let value = match init {
                        Some(e) => self.eval(env, e)?,
                        None => Value::Undefined,
                    };
                    set_var(env, name, value);
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
                // `let` loop variables get a fresh binding per iteration
                // — closures in the body capture this iteration's value,
                // as JavaScript specifies. `var` bindings live in the
                // function frame and are shared.
                let let_names: Vec<String> = match init.as_deref() {
                    Some(Stmt::Let { decls, .. }) => {
                        decls.iter().map(|(name, _)| name.clone()).collect()
                    }
                    _ => Vec::new(),
                };
                loop {
                    if let Some(cond) = cond {
                        if !self.eval(&loop_env, cond)?.is_truthy() {
                            break;
                        }
                    }
                    let iter_env = Env::new(Some(
                        loop_env.parent.clone().unwrap_or_else(|| loop_env.clone()),
                    ));
                    for name in &let_names {
                        if let Some(v) = loop_env.get(name) {
                            iter_env.declare(name, v, false);
                        }
                    }
                    match self.exec_stmt(&iter_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                    // The body may have mutated the iteration's binding;
                    // sync it back before the step runs.
                    for name in &let_names {
                        if let Some(v) = iter_env.get(name) {
                            let _ = loop_env.set(name, v);
                        }
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
            Stmt::ForIn {
                name,
                is_const,
                iterable,
                body,
            } => {
                let target = self.eval(env, iterable)?;
                let keys: Vec<Value> = match &target {
                    Value::Obj(obj) => obj
                        .borrow()
                        .iter()
                        .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
                        .collect(),
                    Value::Arr(arr) => (0..arr.borrow().len())
                        .map(|i| Value::Str(Rc::from(crate::value::fmt_number(i as f64).as_str())))
                        .collect(),
                    Value::Str(s) => (0..s.chars().count())
                        .map(|i| Value::Str(Rc::from(crate::value::fmt_number(i as f64).as_str())))
                        .collect(),
                    // JavaScript: for-in over other types iterates
                    // nothing, without error.
                    _ => Vec::new(),
                };
                for key in keys {
                    let iter_env = Env::new(Some(env.clone()));
                    iter_env.declare(name, key, *is_const);
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
            Expr::Obj(entries) => {
                let mut map: Vec<(String, Value)> = Vec::with_capacity(entries.len());
                for entry in entries {
                    let value = self.eval(env, &entry.value)?;
                    match map.iter_mut().find(|(k, _)| *k == entry.key) {
                        Some(slot) => slot.1 = value,
                        None => map.push((entry.key.clone(), value)),
                    }
                }
                Ok(Value::Obj(Rc::new(RefCell::new(map))))
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
                        Value::Null | Value::Arr(_) | Value::Obj(_) => Value::Str("object".into()),
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
                            // Containers print Node-style; top-level
                            // scalars print raw.
                            line.push_str(&match v {
                                Value::Arr(_) | Value::Obj(_) | Value::Func(_) => v.inspect(),
                                other => other.to_display(),
                            });
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
                let call_env = Env::function_scope(Some(def_env.clone()));
                for (i, p) in params.iter().enumerate() {
                    call_env.declare(p, argv.get(i).cloned().unwrap_or(Value::Undefined), false);
                }
                match body {
                    FnBody::Expr(expr) => self.eval(&call_env, expr),
                    FnBody::Block(stmts) => {
                        self.hoist_fns(&call_env, stmts);
                        for stmt in stmts {
                            hoist_vars(&call_env, stmt);
                        }
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

/// Registers `var` declarations in the nearest function frame before
/// execution reaches them (hoisting): scans `stmt` recursively through
/// control-flow structures, but never into nested functions.
fn hoist_vars(frame: &Rc<Env>, stmt: &Stmt) {
    match stmt {
        Stmt::Var { decls } => {
            for (name, _) in decls {
                if !frame.declares_locally(name) {
                    frame.declare(name, Value::Undefined, false);
                }
            }
        }
        Stmt::Block(stmts) => {
            for stmt in stmts {
                hoist_vars(frame, stmt);
            }
        }
        Stmt::If(_, then, els) => {
            hoist_vars(frame, then);
            if let Some(els) = els {
                hoist_vars(frame, els);
            }
        }
        Stmt::While(_, body) => hoist_vars(frame, body),
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                hoist_vars(frame, init);
            }
            hoist_vars(frame, body);
        }
        Stmt::ForOf { body, .. } | Stmt::ForIn { body, .. } => hoist_vars(frame, body),
        _ => {}
    }
}

/// Assigns to a `var`: walks out to the nearest function-scope frame and
/// updates (or creates) the binding there.
fn set_var(env: &Rc<Env>, name: &str, value: Value) {
    if env.is_function_scope {
        if env.declares_locally(name) {
            let _ = env.set(name, value);
        } else {
            env.declare(name, value, false);
        }
        return;
    }
    match &env.parent {
        Some(parent) => set_var(parent, name, value),
        None => env.declare(name, value, false),
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

/// The string key for an object index, per JavaScript coercion:
/// `obj[1]` is `obj["1"]`.
fn obj_key(index: &Value) -> Option<String> {
    match index {
        Value::Str(s) => Some(s.to_string()),
        Value::Num(n) => Some(crate::value::fmt_number(*n)),
        _ => None,
    }
}

fn index_get(obj: &Value, index: &Value) -> Value {
    if let (Value::Obj(obj), Some(key)) = (obj, obj_key(index)) {
        return obj
            .borrow()
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Undefined);
    }
    match (obj, index) {
        (Value::Arr(arr), i) => match i {
            Value::Num(n) if n.fract() == 0.0 && *n >= 0.0 => arr
                .borrow()
                .get(*n as usize)
                .cloned()
                .unwrap_or(Value::Undefined),
            Value::Str(s) if s.as_ref() == "length" => Value::Num(arr.borrow().len() as f64),
            Value::Str(s) => match s.parse::<f64>() {
                Ok(n) if n.fract() == 0.0 && n >= 0.0 => arr
                    .borrow()
                    .get(n as usize)
                    .cloned()
                    .unwrap_or(Value::Undefined),
                _ => Value::Undefined,
            },
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
    if let (Value::Obj(obj), Some(key)) = (obj, obj_key(index)) {
        let mut obj = obj.borrow_mut();
        match obj.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => obj.push((key, value)),
        }
        return;
    }
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
            return;
        }
    }
    index_set(obj, &Value::Str(Rc::from(prop)), value);
}
