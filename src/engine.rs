//! Main module defining the script evaluation `Engine`.

use crate::any::{Any, AnyExt, Dynamic, Variant};
use crate::parser::{Expr, FnDef, Position, ReturnType, Stmt};
use crate::result::EvalAltResult;
use crate::scope::{Scope, VariableType};

#[cfg(not(feature = "no_optimize"))]
use crate::optimize::OptimizationLevel;

#[cfg(not(feature = "no_index"))]
use crate::INT;

use crate::stdlib::{
    any::{type_name, TypeId},
    borrow::Cow,
    boxed::Box,
    collections::HashMap,
    format,
    iter::once,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

/// An dynamic array of `Dynamic` values.
#[cfg(not(feature = "no_index"))]
pub type Array = Vec<Dynamic>;

pub type FnCallArgs<'a> = Vec<&'a mut Variant>;

pub type FnAny = dyn Fn(FnCallArgs, Position) -> Result<Dynamic, EvalAltResult>;

type IteratorFn = dyn Fn(&Dynamic) -> Box<dyn Iterator<Item = Dynamic>>;

pub(crate) const KEYWORD_PRINT: &'static str = "print";
pub(crate) const KEYWORD_DEBUG: &'static str = "debug";
pub(crate) const KEYWORD_DUMP_AST: &'static str = "dump_ast";
pub(crate) const KEYWORD_TYPE_OF: &'static str = "type_of";
pub(crate) const FUNC_GETTER: &'static str = "get$";
pub(crate) const FUNC_SETTER: &'static str = "set$";

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
#[cfg(not(feature = "no_index"))]
enum IndexSourceType {
    Array,
    String,
    Expression,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct FnSpec<'a> {
    pub name: Cow<'a, str>,
    pub args: Option<Vec<TypeId>>,
}

/// Rhai main scripting engine.
///
/// ```rust
/// # fn main() -> Result<(), rhai::EvalAltResult> {
/// use rhai::Engine;
///
/// let mut engine = Engine::new();
///
/// let result = engine.eval::<i64>("40 + 2")?;
///
/// println!("Answer: {}", result);  // prints 42
/// # Ok(())
/// # }
/// ```
pub struct Engine<'e> {
    /// Optimize the AST after compilation
    #[cfg(not(feature = "no_optimize"))]
    pub(crate) optimization_level: OptimizationLevel,
    /// A hashmap containing all compiled functions known to the engine
    pub(crate) ext_functions: HashMap<FnSpec<'e>, Box<FnAny>>,
    /// A hashmap containing all script-defined functions
    pub(crate) script_functions: Vec<Arc<FnDef>>,
    /// A hashmap containing all iterators known to the engine
    pub(crate) type_iterators: HashMap<TypeId, Box<IteratorFn>>,
    /// A hashmap mapping type names to pretty-print names
    pub(crate) type_names: HashMap<String, String>,

    /// Closure for implementing the print commands
    pub(crate) on_print: Box<dyn FnMut(&str) + 'e>,
    /// Closure for implementing the debug commands
    pub(crate) on_debug: Box<dyn FnMut(&str) + 'e>,
}

impl Engine<'_> {
    /// Create a new `Engine`
    pub fn new() -> Self {
        // User-friendly names for built-in types
        let type_names = [
            #[cfg(not(feature = "no_index"))]
            (type_name::<Array>(), "array"),
            (type_name::<String>(), "string"),
            (type_name::<Dynamic>(), "dynamic"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        // Create the new scripting Engine
        let mut engine = Engine {
            ext_functions: HashMap::new(),
            script_functions: Vec::new(),
            type_iterators: HashMap::new(),
            type_names,
            on_print: Box::new(default_print), // default print/debug implementations
            on_debug: Box::new(default_print),

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(not(feature = "optimize_full"))]
            optimization_level: OptimizationLevel::Simple,

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(feature = "optimize_full")]
            optimization_level: OptimizationLevel::Full,
        };

        engine.register_core_lib();

        #[cfg(not(feature = "no_stdlib"))]
        engine.register_stdlib(); // Register the standard library when no_stdlib is not set

        engine
    }

    /// Control whether and how the `Engine` will optimize an AST after compilation
    #[cfg(not(feature = "no_optimize"))]
    pub fn set_optimization_level(&mut self, optimization_level: OptimizationLevel) {
        self.optimization_level = optimization_level
    }

    /// Call a registered function
    #[cfg(not(feature = "no_optimize"))]
    pub(crate) fn call_ext_fn_raw(
        &self,
        fn_name: &str,
        args: FnCallArgs,
        pos: Position,
    ) -> Result<Option<Dynamic>, EvalAltResult> {
        let spec = FnSpec {
            name: fn_name.into(),
            args: Some(args.iter().map(|a| Any::type_id(&**a)).collect()),
        };

        // Search built-in's and external functions
        if let Some(func) = self.ext_functions.get(&spec) {
            // Run external function
            Ok(Some(func(args, pos)?))
        } else {
            Ok(None)
        }
    }

    /// Universal method for calling functions, that are either
    /// registered with the `Engine` or written in Rhai
    pub(crate) fn call_fn_raw(
        &mut self,
        fn_name: &str,
        args: FnCallArgs,
        def_val: Option<&Dynamic>,
        pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        // First search in script-defined functions (can override built-in)
        if let Ok(n) = self
            .script_functions
            .binary_search_by(|f| f.compare(fn_name, args.len()))
        {
            let mut scope = Scope::new();

            let fn_def = self.script_functions[n].clone();

            scope.extend(
                // Put arguments into scope as variables
                fn_def
                    .params
                    .iter()
                    .zip(args.iter().map(|x| (*x).into_dynamic()))
                    .map(|(name, value)| (name, VariableType::Normal, value)),
            );

            // Evaluate
            // Convert return statement to return value
            return match self.eval_stmt(&mut scope, &fn_def.body) {
                Err(EvalAltResult::Return(x, _)) => Ok(x),
                other => other,
            };
        }

        let spec = FnSpec {
            name: fn_name.into(),
            args: Some(args.iter().map(|a| Any::type_id(&**a)).collect()),
        };

        // Search built-in's and external functions
        if let Some(func) = self.ext_functions.get(&spec) {
            // Run external function
            let result = func(args, pos)?;

            // See if the function match print/debug (which requires special processing)
            let callback = match fn_name {
                KEYWORD_PRINT => self.on_print.as_mut(),
                KEYWORD_DEBUG => self.on_debug.as_mut(),
                _ => return Ok(result),
            };

            let val = &result
                .downcast::<String>()
                .map(|s| *s)
                .unwrap_or("error: not a string".into());

            return Ok(callback(val).into_dynamic());
        }

        if fn_name == KEYWORD_TYPE_OF && args.len() == 1 {
            // Handle `type_of` function
            return Ok(self
                .map_type_name(args[0].type_name())
                .to_string()
                .into_dynamic());
        }

        if fn_name.starts_with(FUNC_GETTER) {
            // Getter function not found
            return Err(EvalAltResult::ErrorDotExpr(
                format!(
                    "- property '{}' unknown or write-only",
                    &fn_name[FUNC_GETTER.len()..]
                ),
                pos,
            ));
        }

        if fn_name.starts_with(FUNC_SETTER) {
            // Setter function not found
            return Err(EvalAltResult::ErrorDotExpr(
                format!(
                    "- property '{}' unknown or read-only",
                    &fn_name[FUNC_SETTER.len()..]
                ),
                pos,
            ));
        }

        if let Some(val) = def_val {
            // Return default value
            return Ok(val.clone());
        }

        // Raise error
        let types_list = args
            .iter()
            .map(|x| (*x).type_name())
            .map(|name| self.map_type_name(name))
            .collect::<Vec<_>>();

        Err(EvalAltResult::ErrorFunctionNotFound(
            format!("{} ({})", fn_name, types_list.join(", ")),
            pos,
        ))
    }

    /// Chain-evaluate a dot setter
    fn get_dot_val_helper(
        &mut self,
        scope: &mut Scope,
        this_ptr: &mut Variant,
        dot_rhs: &Expr,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_rhs {
            // xxx.fn_name(args)
            Expr::FunctionCall(fn_name, arg_expr_list, def_val, pos) => {
                let mut values = arg_expr_list
                    .iter()
                    .map(|arg_expr| self.eval_expr(scope, arg_expr))
                    .collect::<Result<Vec<_>, _>>()?;

                let args = once(this_ptr)
                    .chain(values.iter_mut().map(Dynamic::as_mut))
                    .collect();

                self.call_fn_raw(fn_name, args, def_val.as_ref(), *pos)
            }

            // xxx.id
            Expr::Property(id, pos) => {
                let get_fn_name = format!("{}{}", FUNC_GETTER, id);

                self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)
            }

            // xxx.idx_lhs[idx_expr]
            #[cfg(not(feature = "no_index"))]
            Expr::Index(idx_lhs, idx_expr, idx_pos) => {
                let (val, _) = match idx_lhs.as_ref() {
                    // xxx.id[idx_expr]
                    Expr::Property(id, pos) => {
                        let get_fn_name = format!("{}{}", FUNC_GETTER, id);
                        (
                            self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)?,
                            *pos,
                        )
                    }
                    // xxx.???[???][idx_expr]
                    Expr::Index(_, _, _) => {
                        (self.get_dot_val_helper(scope, this_ptr, idx_lhs)?, *idx_pos)
                    }
                    // Syntax error
                    _ => {
                        return Err(EvalAltResult::ErrorDotExpr(
                            "".to_string(),
                            dot_rhs.position(),
                        ))
                    }
                };

                let idx = self.eval_index_value(scope, idx_expr)?;
                self.get_indexed_value(&val, idx, idx_expr.position(), *idx_pos)
                    .map(|(v, _)| v)
            }

            // xxx.dot_lhs.rhs
            Expr::Dot(dot_lhs, rhs, _) => match dot_lhs.as_ref() {
                // xxx.id.rhs
                Expr::Property(id, pos) => {
                    let get_fn_name = format!("{}{}", FUNC_GETTER, id);

                    self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)
                        .and_then(|mut v| self.get_dot_val_helper(scope, v.as_mut(), rhs))
                }
                // xxx.idx_lhs[idx_expr].rhs
                #[cfg(not(feature = "no_index"))]
                Expr::Index(idx_lhs, idx_expr, idx_pos) => {
                    let (val, _) = match idx_lhs.as_ref() {
                        // xxx.id[idx_expr].rhs
                        Expr::Property(id, pos) => {
                            let get_fn_name = format!("{}{}", FUNC_GETTER, id);
                            (
                                self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)?,
                                *pos,
                            )
                        }
                        // xxx.???[???][idx_expr].rhs
                        Expr::Index(_, _, _) => {
                            (self.get_dot_val_helper(scope, this_ptr, idx_lhs)?, *idx_pos)
                        }
                        // Syntax error
                        _ => {
                            return Err(EvalAltResult::ErrorDotExpr(
                                "".to_string(),
                                dot_rhs.position(),
                            ))
                        }
                    };

                    let idx = self.eval_index_value(scope, idx_expr)?;
                    self.get_indexed_value(&val, idx, idx_expr.position(), *idx_pos)
                        .and_then(|(mut v, _)| self.get_dot_val_helper(scope, v.as_mut(), rhs))
                }
                // Syntax error
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "".to_string(),
                    dot_lhs.position(),
                )),
            },

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "".to_string(),
                dot_rhs.position(),
            )),
        }
    }

    /// Evaluate a dot chain getter
    fn get_dot_val(
        &mut self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_lhs {
            // id.???
            Expr::Variable(id, pos) => {
                let (src_idx, _, mut target) = Self::search_scope(scope, id, Ok, *pos)?;
                let val = self.get_dot_val_helper(scope, target.as_mut(), dot_rhs);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                *scope.get_mut(id, src_idx) = target;

                val
            }

            // idx_lhs[idx_expr].???
            #[cfg(not(feature = "no_index"))]
            Expr::Index(idx_lhs, idx_expr, idx_pos) => {
                let (src_type, src, idx, mut target) =
                    self.eval_index_expr(scope, idx_lhs, idx_expr, *idx_pos)?;
                let val = self.get_dot_val_helper(scope, target.as_mut(), dot_rhs);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                if let Some((id, var_type, src_idx)) = src {
                    match var_type {
                        VariableType::Constant => {
                            return Err(EvalAltResult::ErrorAssignmentToConstant(
                                id.to_string(),
                                idx_lhs.position(),
                            ));
                        }
                        VariableType::Normal => {
                            Self::update_indexed_var_in_scope(
                                src_type,
                                scope,
                                id,
                                src_idx,
                                idx,
                                target,
                                dot_rhs.position(),
                            )?;
                        }
                    }
                }

                val
            }

            // {expr}.???
            expr => {
                let mut target = self.eval_expr(scope, expr)?;
                self.get_dot_val_helper(scope, target.as_mut(), dot_rhs)
            }
        }
    }

    /// Search for a variable within the scope, returning its value and index inside the Scope
    fn search_scope<T>(
        scope: &Scope,
        id: &str,
        map: impl FnOnce(Dynamic) -> Result<T, EvalAltResult>,
        begin: Position,
    ) -> Result<(usize, VariableType, T), EvalAltResult> {
        scope
            .get(id)
            .ok_or_else(|| EvalAltResult::ErrorVariableNotFound(id.into(), begin))
            .and_then(move |(idx, _, var_type, val)| map(val).map(|v| (idx, var_type, v)))
    }

    /// Evaluate the value of an index (must evaluate to INT)
    #[cfg(not(feature = "no_index"))]
    fn eval_index_value(
        &mut self,
        scope: &mut Scope,
        idx_expr: &Expr,
    ) -> Result<INT, EvalAltResult> {
        self.eval_expr(scope, idx_expr)?
            .downcast::<INT>()
            .map(|v| *v)
            .map_err(|_| EvalAltResult::ErrorIndexExpr(idx_expr.position()))
    }

    /// Get the value at the indexed position of a base type
    #[cfg(not(feature = "no_index"))]
    fn get_indexed_value(
        &self,
        val: &Dynamic,
        idx: INT,
        val_pos: Position,
        idx_pos: Position,
    ) -> Result<(Dynamic, IndexSourceType), EvalAltResult> {
        if val.is::<Array>() {
            // val_array[idx]
            let arr = val.downcast_ref::<Array>().expect("array expected");

            if idx >= 0 {
                arr.get(idx as usize)
                    .cloned()
                    .map(|v| (v, IndexSourceType::Array))
                    .ok_or_else(|| EvalAltResult::ErrorArrayBounds(arr.len(), idx, val_pos))
            } else {
                Err(EvalAltResult::ErrorArrayBounds(arr.len(), idx, val_pos))
            }
        } else if val.is::<String>() {
            // val_string[idx]
            let s = val.downcast_ref::<String>().expect("string expected");

            if idx >= 0 {
                s.chars()
                    .nth(idx as usize)
                    .map(|ch| (ch.into_dynamic(), IndexSourceType::String))
                    .ok_or_else(|| {
                        EvalAltResult::ErrorStringBounds(s.chars().count(), idx, val_pos)
                    })
            } else {
                Err(EvalAltResult::ErrorStringBounds(
                    s.chars().count(),
                    idx,
                    val_pos,
                ))
            }
        } else {
            // Error - cannot be indexed
            Err(EvalAltResult::ErrorIndexingType(
                self.map_type_name(val.type_name()).to_string(),
                idx_pos,
            ))
        }
    }

    /// Evaluate an index expression
    #[cfg(not(feature = "no_index"))]
    fn eval_index_expr<'a>(
        &mut self,
        scope: &mut Scope,
        lhs: &'a Expr,
        idx_expr: &Expr,
        idx_pos: Position,
    ) -> Result<
        (
            IndexSourceType,
            Option<(&'a str, VariableType, usize)>,
            usize,
            Dynamic,
        ),
        EvalAltResult,
    > {
        let idx = self.eval_index_value(scope, idx_expr)?;

        match lhs {
            // id[idx_expr]
            Expr::Variable(id, _) => Self::search_scope(
                scope,
                &id,
                |val| self.get_indexed_value(&val, idx, idx_expr.position(), idx_pos),
                lhs.position(),
            )
            .map(|(src_idx, var_type, (val, src_type))| {
                (
                    src_type,
                    Some((id.as_str(), var_type, src_idx)),
                    idx as usize,
                    val,
                )
            }),

            // (expr)[idx_expr]
            expr => {
                let val = self.eval_expr(scope, expr)?;

                self.get_indexed_value(&val, idx, idx_expr.position(), idx_pos)
                    .map(|(v, _)| (IndexSourceType::Expression, None, idx as usize, v))
            }
        }
    }

    /// Replace a character at an index position in a mutable string
    #[cfg(not(feature = "no_index"))]
    fn str_replace_char(s: &mut String, idx: usize, new_ch: char) {
        let mut chars: Vec<char> = s.chars().collect();
        let ch = *chars.get(idx).expect("string index out of bounds");

        // See if changed - if so, update the String
        if ch != new_ch {
            chars[idx] = new_ch;
            s.clear();
            chars.iter().for_each(|&ch| s.push(ch));
        }
    }

    /// Update the value at an index position in a variable inside the scope
    #[cfg(not(feature = "no_index"))]
    fn update_indexed_var_in_scope(
        src_type: IndexSourceType,
        scope: &mut Scope,
        id: &str,
        src_idx: usize,
        idx: usize,
        new_val: Dynamic,
        val_pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        match src_type {
            // array_id[idx] = val
            IndexSourceType::Array => {
                let arr = scope.get_mut_by_type::<Array>(id, src_idx);
                Ok((arr[idx as usize] = new_val).into_dynamic())
            }

            // string_id[idx] = val
            IndexSourceType::String => {
                let s = scope.get_mut_by_type::<String>(id, src_idx);
                // Value must be a character
                let ch = *new_val
                    .downcast::<char>()
                    .map_err(|_| EvalAltResult::ErrorCharMismatch(val_pos))?;
                Ok(Self::str_replace_char(s, idx as usize, ch).into_dynamic())
            }

            IndexSourceType::Expression => panic!("expression cannot be indexed for update"),
        }
    }

    /// Update the value at an index position
    #[cfg(not(feature = "no_index"))]
    fn update_indexed_value(
        mut target: Dynamic,
        idx: usize,
        new_val: Dynamic,
        pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        if target.is::<Array>() {
            let arr = target.downcast_mut::<Array>().expect("array expected");
            arr[idx as usize] = new_val;
        } else if target.is::<String>() {
            let s = target.downcast_mut::<String>().expect("string expected");
            // Value must be a character
            let ch = *new_val
                .downcast::<char>()
                .map_err(|_| EvalAltResult::ErrorCharMismatch(pos))?;
            Self::str_replace_char(s, idx as usize, ch);
        } else {
            // All other variable types should be an error
            panic!("array or string source type expected for indexing")
        }

        Ok(target)
    }

    /// Chain-evaluate a dot setter
    fn set_dot_val_helper(
        &mut self,
        scope: &mut Scope,
        this_ptr: &mut Variant,
        dot_rhs: &Expr,
        mut new_val: Dynamic,
        val_pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_rhs {
            // xxx.id
            Expr::Property(id, pos) => {
                let set_fn_name = format!("{}{}", FUNC_SETTER, id);

                self.call_fn_raw(&set_fn_name, vec![this_ptr, new_val.as_mut()], None, *pos)
            }

            // xxx.lhs[idx_expr]
            // TODO - Allow chaining of indexing!
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, idx_pos) => match lhs.as_ref() {
                // xxx.id[idx_expr]
                Expr::Property(id, pos) => {
                    let get_fn_name = format!("{}{}", FUNC_GETTER, id);

                    self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)
                        .and_then(|v| {
                            let idx = self.eval_index_value(scope, idx_expr)?;
                            Self::update_indexed_value(v, idx as usize, new_val, val_pos)
                        })
                        .and_then(|mut v| {
                            let set_fn_name = format!("{}{}", FUNC_SETTER, id);
                            self.call_fn_raw(&set_fn_name, vec![this_ptr, v.as_mut()], None, *pos)
                        })
                }

                // All others - syntax error for setters chain
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "for assignment".to_string(),
                    *idx_pos,
                )),
            },

            // xxx.lhs.{...}
            Expr::Dot(lhs, rhs, _) => match lhs.as_ref() {
                // xxx.id.rhs
                Expr::Property(id, pos) => {
                    let get_fn_name = format!("{}{}", FUNC_GETTER, id);

                    self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)
                        .and_then(|mut v| {
                            self.set_dot_val_helper(scope, v.as_mut(), rhs, new_val, val_pos)
                                .map(|_| v) // Discard Ok return value
                        })
                        .and_then(|mut v| {
                            let set_fn_name = format!("{}{}", FUNC_SETTER, id);

                            self.call_fn_raw(&set_fn_name, vec![this_ptr, v.as_mut()], None, *pos)
                        })
                }

                // xxx.lhs[idx_expr].rhs
                // TODO - Allow chaining of indexing!
                #[cfg(not(feature = "no_index"))]
                Expr::Index(lhs, idx_expr, idx_pos) => match lhs.as_ref() {
                    // xxx.id[idx_expr].rhs
                    Expr::Property(id, pos) => {
                        let get_fn_name = format!("{}{}", FUNC_GETTER, id);

                        self.call_fn_raw(&get_fn_name, vec![this_ptr], None, *pos)
                            .and_then(|v| {
                                let idx = self.eval_index_value(scope, idx_expr)?;
                                let (mut target, _) =
                                    self.get_indexed_value(&v, idx, idx_expr.position(), *idx_pos)?;

                                self.set_dot_val_helper(
                                    scope,
                                    target.as_mut(),
                                    rhs,
                                    new_val,
                                    val_pos,
                                )?;

                                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                                Self::update_indexed_value(v, idx as usize, target, val_pos)
                            })
                            .and_then(|mut v| {
                                let set_fn_name = format!("{}{}", FUNC_SETTER, id);

                                self.call_fn_raw(
                                    &set_fn_name,
                                    vec![this_ptr, v.as_mut()],
                                    None,
                                    *pos,
                                )
                            })
                    }

                    // All others - syntax error for setters chain
                    _ => Err(EvalAltResult::ErrorDotExpr(
                        "for assignment".to_string(),
                        *idx_pos,
                    )),
                },

                // All others - syntax error for setters chain
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "for assignment".to_string(),
                    lhs.position(),
                )),
            },

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "for assignment".to_string(),
                dot_rhs.position(),
            )),
        }
    }

    // Evaluate a dot chain setter
    fn set_dot_val(
        &mut self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
        new_val: Dynamic,
        val_pos: Position,
        op_pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_lhs {
            // id.???
            Expr::Variable(id, pos) => {
                let (src_idx, var_type, mut target) = Self::search_scope(scope, id, Ok, *pos)?;

                match var_type {
                    VariableType::Constant => {
                        return Err(EvalAltResult::ErrorAssignmentToConstant(
                            id.to_string(),
                            op_pos,
                        ))
                    }
                    _ => (),
                }

                let val =
                    self.set_dot_val_helper(scope, target.as_mut(), dot_rhs, new_val, val_pos);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                *scope.get_mut(id, src_idx) = target;

                val
            }

            // lhs[idx_expr].???
            // TODO - Allow chaining of indexing!
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, idx_pos) => {
                let (src_type, src, idx, mut target) =
                    self.eval_index_expr(scope, lhs, idx_expr, *idx_pos)?;
                let val =
                    self.set_dot_val_helper(scope, target.as_mut(), dot_rhs, new_val, val_pos);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                if let Some((id, var_type, src_idx)) = src {
                    match var_type {
                        VariableType::Constant => {
                            return Err(EvalAltResult::ErrorAssignmentToConstant(
                                id.to_string(),
                                lhs.position(),
                            ));
                        }
                        VariableType::Normal => {
                            Self::update_indexed_var_in_scope(
                                src_type, scope, id, src_idx, idx, target, val_pos,
                            )?;
                        }
                    }
                }

                val
            }

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "for assignment".to_string(),
                dot_lhs.position(),
            )),
        }
    }

    /// Evaluate an expression
    fn eval_expr(&mut self, scope: &mut Scope, expr: &Expr) -> Result<Dynamic, EvalAltResult> {
        match expr {
            #[cfg(not(feature = "no_float"))]
            Expr::FloatConstant(f, _) => Ok(f.into_dynamic()),

            Expr::IntegerConstant(i, _) => Ok(i.into_dynamic()),
            Expr::StringConstant(s, _) => Ok(s.into_dynamic()),
            Expr::CharConstant(c, _) => Ok(c.into_dynamic()),
            Expr::Variable(id, pos) => {
                Self::search_scope(scope, id, Ok, *pos).map(|(_, _, val)| val)
            }
            Expr::Property(_, _) => panic!("unexpected property."),

            // lhs[idx_expr]
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, idx_pos) => self
                .eval_index_expr(scope, lhs, idx_expr, *idx_pos)
                .map(|(_, _, _, x)| x),

            // Statement block
            Expr::Stmt(stmt, _) => self.eval_stmt(scope, stmt),

            // lhs = rhs
            Expr::Assignment(lhs, rhs, op_pos) => {
                let rhs_val = self.eval_expr(scope, rhs)?;

                match lhs.as_ref() {
                    // name = rhs
                    Expr::Variable(name, pos) => match scope.get(name) {
                        Some((idx, _, VariableType::Normal, _)) => {
                            *scope.get_mut(name, idx) = rhs_val.clone();
                            Ok(rhs_val)
                        }
                        Some((_, _, VariableType::Constant, _)) => Err(
                            EvalAltResult::ErrorAssignmentToConstant(name.to_string(), *op_pos),
                        ),
                        _ => Err(EvalAltResult::ErrorVariableNotFound(name.clone(), *pos)),
                    },

                    // idx_lhs[idx_expr] = rhs
                    #[cfg(not(feature = "no_index"))]
                    Expr::Index(idx_lhs, idx_expr, idx_pos) => {
                        let (src_type, src, idx, _) =
                            self.eval_index_expr(scope, idx_lhs, idx_expr, *idx_pos)?;

                        if let Some((id, var_type, src_idx)) = src {
                            match var_type {
                                VariableType::Constant => {
                                    return Err(EvalAltResult::ErrorAssignmentToConstant(
                                        id.to_string(),
                                        idx_lhs.position(),
                                    ));
                                }
                                VariableType::Normal => Ok(Self::update_indexed_var_in_scope(
                                    src_type,
                                    scope,
                                    &id,
                                    src_idx,
                                    idx,
                                    rhs_val,
                                    rhs.position(),
                                )?),
                            }
                        } else {
                            Err(EvalAltResult::ErrorAssignmentToUnknownLHS(
                                idx_lhs.position(),
                            ))
                        }
                    }

                    // dot_lhs.dot_rhs = rhs
                    Expr::Dot(dot_lhs, dot_rhs, _) => {
                        self.set_dot_val(scope, dot_lhs, dot_rhs, rhs_val, rhs.position(), *op_pos)
                    }

                    // Error assignment to constant
                    expr if expr.is_constant() => Err(EvalAltResult::ErrorAssignmentToConstant(
                        expr.get_constant_str(),
                        lhs.position(),
                    )),

                    // Syntax error
                    _ => Err(EvalAltResult::ErrorAssignmentToUnknownLHS(lhs.position())),
                }
            }

            Expr::Dot(lhs, rhs, _) => self.get_dot_val(scope, lhs, rhs),

            #[cfg(not(feature = "no_index"))]
            Expr::Array(contents, _) => {
                let mut arr = Vec::new();

                for item in contents {
                    arr.push(self.eval_expr(scope, item)?);
                }

                Ok(Box::new(arr))
            }

            // Dump AST
            Expr::FunctionCall(fn_name, args_expr_list, _, pos) if fn_name == KEYWORD_DUMP_AST => {
                let pos = if args_expr_list.len() == 0 {
                    *pos
                } else {
                    args_expr_list[0].position()
                };

                // Change the argument to a debug dump of the expressions
                let result = args_expr_list
                    .into_iter()
                    .map(|expr| format!("{:#?}", expr))
                    .collect::<Vec<_>>()
                    .join("\n");

                // Redirect call to `print`
                self.call_fn_raw(
                    KEYWORD_PRINT,
                    vec![result.into_dynamic().as_mut()],
                    None,
                    pos,
                )
            }

            // Normal function call
            Expr::FunctionCall(fn_name, args_expr_list, def_val, pos) => {
                let mut values = args_expr_list
                    .iter()
                    .map(|expr| self.eval_expr(scope, expr))
                    .collect::<Result<Vec<Dynamic>, _>>()?;

                self.call_fn_raw(
                    fn_name,
                    values.iter_mut().map(|b| b.as_mut()).collect(),
                    def_val.as_ref(),
                    *pos,
                )
            }

            Expr::And(lhs, rhs) => Ok(Box::new(
                *self
                    .eval_expr(scope, &*lhs)?
                    .downcast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), lhs.position())
                    })?
                    && // Short-circuit using &&
                *self
                    .eval_expr(scope, &*rhs)?
                    .downcast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), rhs.position())
                    })?,
            )),

            Expr::Or(lhs, rhs) => Ok(Box::new(
                *self
                    .eval_expr(scope, &*lhs)?
                    .downcast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), lhs.position())
                    })?
                    || // Short-circuit using ||
                *self
                    .eval_expr(scope, &*rhs)?
                    .downcast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), rhs.position())
                    })?,
            )),

            Expr::True(_) => Ok(true.into_dynamic()),
            Expr::False(_) => Ok(false.into_dynamic()),
            Expr::Unit(_) => Ok(().into_dynamic()),
        }
    }

    /// Evaluate a statement
    pub(crate) fn eval_stmt(
        &mut self,
        scope: &mut Scope,
        stmt: &Stmt,
    ) -> Result<Dynamic, EvalAltResult> {
        match stmt {
            // No-op
            Stmt::Noop(_) => Ok(().into_dynamic()),

            // Expression as statement
            Stmt::Expr(expr) => {
                let result = self.eval_expr(scope, expr)?;

                Ok(if !matches!(expr.as_ref(), Expr::Assignment(_, _, _)) {
                    result
                } else {
                    // If it is an assignment, erase the result at the root
                    ().into_dynamic()
                })
            }

            // Block scope
            Stmt::Block(block, _) => {
                let prev_len = scope.len();
                let mut result: Result<Dynamic, EvalAltResult> = Ok(().into_dynamic());

                for stmt in block.iter() {
                    result = self.eval_stmt(scope, stmt);

                    if result.is_err() {
                        break;
                    }
                }

                scope.rewind(prev_len);

                result
            }

            // If-else statement
            Stmt::IfElse(guard, if_body, else_body) => self
                .eval_expr(scope, guard)?
                .downcast::<bool>()
                .map_err(|_| EvalAltResult::ErrorIfGuard(guard.position()))
                .and_then(|guard_val| {
                    if *guard_val {
                        self.eval_stmt(scope, if_body)
                    } else if let Some(stmt) = else_body {
                        self.eval_stmt(scope, stmt.as_ref())
                    } else {
                        Ok(().into_dynamic())
                    }
                }),

            // While loop
            Stmt::While(guard, body) => loop {
                match self.eval_expr(scope, guard)?.downcast::<bool>() {
                    Ok(guard_val) => {
                        if *guard_val {
                            match self.eval_stmt(scope, body) {
                                Ok(_) => (),
                                Err(EvalAltResult::ErrorLoopBreak(_)) => {
                                    return Ok(().into_dynamic())
                                }
                                Err(x) => return Err(x),
                            }
                        } else {
                            return Ok(().into_dynamic());
                        }
                    }
                    Err(_) => return Err(EvalAltResult::ErrorIfGuard(guard.position())),
                }
            },

            // Loop statement
            Stmt::Loop(body) => loop {
                match self.eval_stmt(scope, body) {
                    Ok(_) => (),
                    Err(EvalAltResult::ErrorLoopBreak(_)) => return Ok(().into_dynamic()),
                    Err(x) => return Err(x),
                }
            },

            // For loop
            Stmt::For(name, expr, body) => {
                let arr = self.eval_expr(scope, expr)?;
                let tid = Any::type_id(&*arr);

                if let Some(iter_fn) = self.type_iterators.get(&tid) {
                    scope.push(name.clone(), ());
                    let idx = scope.len() - 1;

                    for a in iter_fn(&arr) {
                        *scope.get_mut(name, idx) = a;

                        match self.eval_stmt(scope, body) {
                            Ok(_) => (),
                            Err(EvalAltResult::ErrorLoopBreak(_)) => break,
                            Err(x) => return Err(x),
                        }
                    }
                    scope.pop();
                    Ok(().into_dynamic())
                } else {
                    return Err(EvalAltResult::ErrorFor(expr.position()));
                }
            }

            // Break statement
            Stmt::Break(pos) => Err(EvalAltResult::ErrorLoopBreak(*pos)),

            // Empty return
            Stmt::ReturnWithVal(None, ReturnType::Return, pos) => {
                Err(EvalAltResult::Return(().into_dynamic(), *pos))
            }

            // Return value
            Stmt::ReturnWithVal(Some(a), ReturnType::Return, pos) => {
                Err(EvalAltResult::Return(self.eval_expr(scope, a)?, *pos))
            }

            // Empty throw
            Stmt::ReturnWithVal(None, ReturnType::Exception, pos) => {
                Err(EvalAltResult::ErrorRuntime("".into(), *pos))
            }

            // Throw value
            Stmt::ReturnWithVal(Some(a), ReturnType::Exception, pos) => {
                let val = self.eval_expr(scope, a)?;
                Err(EvalAltResult::ErrorRuntime(
                    val.downcast::<String>()
                        .map(|s| *s)
                        .unwrap_or("".to_string()),
                    *pos,
                ))
            }

            // Let statement
            Stmt::Let(name, Some(expr), _) => {
                let val = self.eval_expr(scope, expr)?;
                scope.push_dynamic(name.clone(), VariableType::Normal, val);
                Ok(().into_dynamic())
            }

            Stmt::Let(name, None, _) => {
                scope.push(name.clone(), ());
                Ok(().into_dynamic())
            }

            // Const statement
            Stmt::Const(name, expr, _) if expr.is_constant() => {
                let val = self.eval_expr(scope, expr)?;
                scope.push_dynamic(name.clone(), VariableType::Constant, val);
                Ok(().into_dynamic())
            }

            Stmt::Const(_, _, _) => panic!("constant expression not constant!"),
        }
    }

    /// Map a type_name into a pretty-print name
    pub(crate) fn map_type_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.type_names
            .get(name)
            .map(|s| s.as_str())
            .unwrap_or(name)
    }

    /// Clean up all script-defined functions within the `Engine`.
    pub fn clear_functions(&mut self) {
        self.script_functions.clear();
    }
}

/// Print/debug to stdout
#[cfg(not(feature = "no_std"))]
#[cfg(not(feature = "no_stdlib"))]
fn default_print(s: &str) {
    println!("{}", s);
}

/// No-op
#[cfg(any(feature = "no_std", feature = "no_stdlib"))]
fn default_print(_: &str) {}
