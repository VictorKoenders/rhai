//! Module defining external-loaded modules for Rhai.

use crate::ast::{FnAccess, Ident};
use crate::dynamic::Variant;
use crate::fn_native::{shared_take_or_clone, CallableFunction, FnCallArgs, IteratorFn, SendSync};
use crate::fn_register::by_value as cast_arg;
use crate::stdlib::{
    any::TypeId,
    boxed::Box,
    collections::HashMap,
    fmt, format,
    iter::empty,
    num::NonZeroU64,
    num::NonZeroUsize,
    ops::{Add, AddAssign, Deref, DerefMut},
    string::{String, ToString},
    vec::Vec,
};
use crate::token::Token;
use crate::utils::{combine_hashes, StraightHasherBuilder};
use crate::{
    Dynamic, EvalAltResult, ImmutableString, NativeCallContext, Position, Shared, StaticVec,
};

#[cfg(not(feature = "no_function"))]
use crate::ast::ScriptFnDef;

#[cfg(not(feature = "no_index"))]
use crate::Array;

#[cfg(not(feature = "no_index"))]
#[cfg(not(feature = "no_object"))]
use crate::Map;

/// A type representing the namespace of a function.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum FnNamespace {
    /// Expose to global namespace.
    Global,
    /// Module namespace only.
    Internal,
}

impl Default for FnNamespace {
    fn default() -> Self {
        Self::Internal
    }
}

impl FnNamespace {
    /// Is this namespace [global][FnNamespace::Global]?
    #[inline(always)]
    pub fn is_global(self) -> bool {
        match self {
            Self::Global => true,
            Self::Internal => false,
        }
    }
    /// Is this namespace [internal][FnNamespace::Internal]?
    #[inline(always)]
    pub fn is_internal(self) -> bool {
        match self {
            Self::Global => false,
            Self::Internal => true,
        }
    }
}

/// Data structure containing a single registered function.
#[derive(Debug, Clone)]
pub struct FuncInfo {
    /// Function instance.
    pub func: CallableFunction,
    /// Function namespace.
    pub namespace: FnNamespace,
    /// Function access mode.
    pub access: FnAccess,
    /// Function name.
    pub name: String,
    /// Number of parameters.
    pub params: usize,
    /// Parameter types (if applicable).
    pub param_types: StaticVec<TypeId>,
    /// Parameter names (if available).
    pub param_names: StaticVec<ImmutableString>,
}

impl FuncInfo {
    /// Generate a signature of the function.
    pub fn gen_signature(&self) -> String {
        let mut sig = format!("{}(", self.name);

        if !self.param_names.is_empty() {
            let mut params: Vec<_> = self
                .param_names
                .iter()
                .map(ImmutableString::to_string)
                .collect();
            let return_type = params.pop().unwrap_or_else(|| "()".to_string());
            sig.push_str(&params.join(", "));
            if return_type != "()" {
                sig.push_str(") -> ");
                sig.push_str(&return_type);
            } else {
                sig.push_str(")");
            }
        } else {
            for x in 0..self.params {
                sig.push_str("_");
                if x < self.params - 1 {
                    sig.push_str(", ");
                }
            }

            if self.func.is_script() {
                sig.push_str(")");
            } else {
                sig.push_str(") -> ?");
            }
        }

        sig
    }
}

/// A module which may contain variables, sub-modules, external Rust functions,
/// and/or script-defined functions.
#[derive(Clone)]
pub struct Module {
    /// ID identifying the module.
    id: Option<ImmutableString>,
    /// Sub-modules.
    modules: HashMap<ImmutableString, Shared<Module>>,
    /// [`Module`] variables.
    variables: HashMap<ImmutableString, Dynamic>,
    /// Flattened collection of all [`Module`] variables, including those in sub-modules.
    all_variables: HashMap<NonZeroU64, Dynamic, StraightHasherBuilder>,
    /// External Rust functions.
    functions: HashMap<NonZeroU64, FuncInfo, StraightHasherBuilder>,
    /// Flattened collection of all external Rust functions, native or scripted.
    /// including those in sub-modules.
    all_functions: HashMap<NonZeroU64, CallableFunction, StraightHasherBuilder>,
    /// Iterator functions, keyed by the type producing the iterator.
    type_iterators: HashMap<TypeId, IteratorFn>,
    /// Flattened collection of iterator functions, including those in sub-modules.
    all_type_iterators: HashMap<TypeId, IteratorFn>,
    /// Is the [`Module`] indexed?
    indexed: bool,
}

impl Default for Module {
    fn default() -> Self {
        Self {
            id: None,
            modules: Default::default(),
            variables: Default::default(),
            all_variables: Default::default(),
            functions: HashMap::with_capacity_and_hasher(64, StraightHasherBuilder),
            all_functions: HashMap::with_capacity_and_hasher(256, StraightHasherBuilder),
            type_iterators: Default::default(),
            all_type_iterators: Default::default(),
            indexed: false,
        }
    }
}

impl fmt::Debug for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Module({}\n    modules: {}\n    vars: {}\n    functions: {}\n)",
            if let Some(ref id) = self.id {
                format!("id: {:?}", id)
            } else {
                "".to_string()
            },
            self.modules
                .keys()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.variables
                .iter()
                .map(|(k, v)| format!("{}={:?}", k, v))
                .collect::<Vec<_>>()
                .join(", "),
            self.functions
                .values()
                .map(|FuncInfo { func, .. }| func.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

impl AsRef<Module> for Module {
    #[inline(always)]
    fn as_ref(&self) -> &Module {
        self
    }
}

impl Module {
    /// Create a new [`Module`].
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert_eq!(module.get_var_value::<i64>("answer").unwrap(), 42);
    /// ```
    #[inline(always)]
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a new [`Module`] with a specified capacity for native Rust functions.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert_eq!(module.get_var_value::<i64>("answer").unwrap(), 42);
    /// ```
    #[inline(always)]
    pub fn new_with_capacity(capacity: usize) -> Self {
        Self {
            functions: HashMap::with_capacity_and_hasher(capacity, StraightHasherBuilder),
            ..Default::default()
        }
    }

    /// Get the ID of the [`Module`], if any.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_id(Some("hello"));
    /// assert_eq!(module.id(), Some("hello"));
    /// ```
    #[inline(always)]
    pub fn id(&self) -> Option<&str> {
        self.id_raw().map(|s| s.as_str())
    }

    /// Get the ID of the [`Module`] as an [`ImmutableString`], if any.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_id(Some("hello"));
    /// assert_eq!(module.id_raw().map(|s| s.as_str()), Some("hello"));
    /// ```
    #[inline(always)]
    pub fn id_raw(&self) -> Option<&ImmutableString> {
        self.id.as_ref()
    }

    /// Set the ID of the [`Module`].
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_id(Some("hello"));
    /// assert_eq!(module.id(), Some("hello"));
    /// ```
    #[inline(always)]
    pub fn set_id<S: Into<ImmutableString>>(&mut self, id: Option<S>) {
        self.id = id.map(|s| s.into());
    }

    /// Is the [`Module`] empty?
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let module = Module::new();
    /// assert!(module.is_empty());
    /// ```
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
            && self.all_functions.is_empty()
            && self.variables.is_empty()
            && self.all_variables.is_empty()
            && self.modules.is_empty()
            && self.type_iterators.is_empty()
            && self.all_type_iterators.is_empty()
    }

    /// Is the [`Module`] indexed?
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// assert!(!module.is_indexed());
    ///
    /// # #[cfg(not(feature = "no_module"))]
    /// # {
    /// module.build_index();
    /// assert!(module.is_indexed());
    /// # }
    /// ```
    #[inline(always)]
    pub fn is_indexed(&self) -> bool {
        self.indexed
    }

    /// Generate signatures for all the functions in the [`Module`].
    #[inline(always)]
    pub fn gen_fn_signatures<'a>(&'a self) -> impl Iterator<Item = String> + 'a {
        self.functions
            .values()
            .filter(|FuncInfo { access, .. }| !access.is_private())
            .map(FuncInfo::gen_signature)
    }

    /// Does a variable exist in the [`Module`]?
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert!(module.contains_var("answer"));
    /// ```
    #[inline(always)]
    pub fn contains_var(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    /// Get the value of a [`Module`] variable.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert_eq!(module.get_var_value::<i64>("answer").unwrap(), 42);
    /// ```
    #[inline(always)]
    pub fn get_var_value<T: Variant + Clone>(&self, name: &str) -> Option<T> {
        self.get_var(name).and_then(Dynamic::try_cast::<T>)
    }

    /// Get a [`Module`] variable as a [`Dynamic`].
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert_eq!(module.get_var("answer").unwrap().cast::<i64>(), 42);
    /// ```
    #[inline(always)]
    pub fn get_var(&self, name: &str) -> Option<Dynamic> {
        self.variables.get(name).cloned()
    }

    /// Set a variable into the [`Module`].
    ///
    /// If there is an existing variable of the same name, it is replaced.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// module.set_var("answer", 42_i64);
    /// assert_eq!(module.get_var_value::<i64>("answer").unwrap(), 42);
    /// ```
    #[inline(always)]
    pub fn set_var(
        &mut self,
        name: impl Into<ImmutableString>,
        value: impl Variant + Clone,
    ) -> &mut Self {
        self.variables.insert(name.into(), Dynamic::from(value));
        self.indexed = false;
        self
    }

    /// Get a reference to a namespace-qualified variable.
    /// Name and Position in [`EvalAltResult`] are [`None`] and [`NONE`][Position::NONE] and must be set afterwards.
    ///
    /// The [`NonZeroU64`] hash is calculated by the function [`calc_native_fn_hash`][crate::calc_native_fn_hash].
    #[inline(always)]
    pub(crate) fn get_qualified_var(
        &self,
        hash_var: NonZeroU64,
    ) -> Result<&Dynamic, Box<EvalAltResult>> {
        self.all_variables.get(&hash_var).ok_or_else(|| {
            EvalAltResult::ErrorVariableNotFound(String::new(), Position::NONE).into()
        })
    }

    /// Set a script-defined function into the [`Module`].
    ///
    /// If there is an existing function of the same name and number of arguments, it is replaced.
    #[cfg(not(feature = "no_function"))]
    #[inline]
    pub(crate) fn set_script_fn(&mut self, fn_def: impl Into<Shared<ScriptFnDef>>) -> NonZeroU64 {
        let fn_def = fn_def.into();

        // None + function name + number of arguments.
        let num_params = fn_def.params.len();
        let hash_script = crate::calc_script_fn_hash(empty(), &fn_def.name, num_params).unwrap();
        let mut param_names: StaticVec<_> = fn_def.params.iter().cloned().collect();
        param_names.push("Dynamic".into());
        self.functions.insert(
            hash_script,
            FuncInfo {
                name: fn_def.name.to_string(),
                namespace: FnNamespace::Internal,
                access: fn_def.access,
                params: num_params,
                param_types: Default::default(),
                param_names,
                func: fn_def.into(),
            },
        );
        self.indexed = false;
        hash_script
    }

    /// Get a script-defined function in the [`Module`] based on name and number of parameters.
    #[cfg(not(feature = "no_function"))]
    #[inline(always)]
    pub fn get_script_fn(
        &self,
        name: &str,
        num_params: usize,
        public_only: bool,
    ) -> Option<&ScriptFnDef> {
        self.functions
            .values()
            .find(
                |FuncInfo {
                     name: fn_name,
                     access,
                     params,
                     ..
                 }| {
                    (!public_only || *access == FnAccess::Public)
                        && *params == num_params
                        && fn_name == name
                },
            )
            .map(|FuncInfo { func, .. }| func.get_fn_def())
    }

    /// Get a mutable reference to the underlying [`HashMap`] of sub-modules.
    ///
    /// # WARNING
    ///
    /// By taking a mutable reference, it is assumed that some sub-modules will be modified.
    /// Thus the [`Module`] is automatically set to be non-indexed.
    #[cfg(not(feature = "no_module"))]
    #[inline(always)]
    pub(crate) fn sub_modules_mut(&mut self) -> &mut HashMap<ImmutableString, Shared<Module>> {
        // We must assume that the user has changed the sub-modules
        // (otherwise why take a mutable reference?)
        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;

        &mut self.modules
    }

    /// Does a sub-module exist in the [`Module`]?
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let sub_module = Module::new();
    /// module.set_sub_module("question", sub_module);
    /// assert!(module.contains_sub_module("question"));
    /// ```
    #[inline(always)]
    pub fn contains_sub_module(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// Get a sub-module in the [`Module`].
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let sub_module = Module::new();
    /// module.set_sub_module("question", sub_module);
    /// assert!(module.get_sub_module("question").is_some());
    /// ```
    #[inline(always)]
    pub fn get_sub_module(&self, name: &str) -> Option<&Module> {
        self.modules.get(name).map(|m| m.as_ref())
    }

    /// Set a sub-module into the [`Module`].
    ///
    /// If there is an existing sub-module of the same name, it is replaced.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let sub_module = Module::new();
    /// module.set_sub_module("question", sub_module);
    /// assert!(module.get_sub_module("question").is_some());
    /// ```
    #[inline(always)]
    pub fn set_sub_module(
        &mut self,
        name: impl Into<ImmutableString>,
        sub_module: impl Into<Shared<Module>>,
    ) -> &mut Self {
        self.modules.insert(name.into(), sub_module.into());
        self.indexed = false;
        self
    }

    /// Does the particular Rust function exist in the [`Module`]?
    ///
    /// The [`NonZeroU64`] hash is calculated by the function [`calc_native_fn_hash`][crate::calc_native_fn_hash].
    /// It is also returned by the `set_fn_XXX` calls.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_0("calc", || Ok(42_i64));
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline]
    pub fn contains_fn(&self, hash_fn: NonZeroU64, public_only: bool) -> bool {
        if public_only {
            self.functions
                .get(&hash_fn)
                .map(|FuncInfo { access, .. }| access.is_public())
                .unwrap_or(false)
        } else {
            self.functions.contains_key(&hash_fn)
        }
    }

    /// Update the metadata (parameter names/types and return type) of a registered function.
    ///
    /// The [`NonZeroU64`] hash is calculated either by the function
    /// [`calc_native_fn_hash`][crate::calc_native_fn_hash] or the function
    /// [`calc_script_fn_hash`][crate::calc_script_fn_hash].
    ///
    /// ## Parameter Names and Types
    ///
    /// Each parameter name/type pair should be a single string of the format: `var_name: type`.
    ///
    /// ## Return Type
    ///
    /// The _last entry_ in the list should be the _return type_ of the function.
    /// In other words, the number of entries should be one larger than the number of parameters.
    #[inline(always)]
    pub fn update_fn_metadata<'a>(
        &mut self,
        hash_fn: NonZeroU64,
        arg_names: impl AsRef<[&'a str]>,
    ) -> &mut Self {
        if let Some(f) = self.functions.get_mut(&hash_fn) {
            f.param_names = arg_names.as_ref().iter().map(|&n| n.into()).collect();
        }
        self
    }

    /// Update the namespace of a registered function.
    ///
    /// The [`NonZeroU64`] hash is calculated either by the function
    /// [`calc_native_fn_hash`][crate::calc_native_fn_hash] or the function
    /// [`calc_script_fn_hash`][crate::calc_script_fn_hash].
    #[inline(always)]
    pub fn update_fn_namespace(
        &mut self,
        hash_fn: NonZeroU64,
        namespace: FnNamespace,
    ) -> &mut Self {
        if let Some(f) = self.functions.get_mut(&hash_fn) {
            f.namespace = namespace;
        }
        self.indexed = false;
        self
    }

    /// Set a Rust function into the [`Module`], returning a hash key.
    ///
    /// If there is an existing Rust function of the same hash, it is replaced.
    ///
    /// # WARNING - Low Level API
    ///
    /// This function is very low level.
    #[inline]
    pub fn set_fn(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        access: FnAccess,
        arg_names: Option<&[&str]>,
        arg_types: &[TypeId],
        func: CallableFunction,
    ) -> NonZeroU64 {
        let name = name.into();

        let hash_fn =
            crate::calc_native_fn_hash(empty(), &name, arg_types.iter().cloned()).unwrap();

        let param_types = arg_types
            .into_iter()
            .cloned()
            .map(|id| {
                if id == TypeId::of::<&str>() || id == TypeId::of::<String>() {
                    TypeId::of::<ImmutableString>()
                } else {
                    id
                }
            })
            .collect::<StaticVec<_>>();

        self.functions.insert(
            hash_fn,
            FuncInfo {
                name,
                namespace,
                access,
                params: param_types.len(),
                param_types,
                param_names: if let Some(p) = arg_names {
                    p.iter().map(|&v| v.into()).collect()
                } else {
                    Default::default()
                },
                func: func.into(),
            },
        );

        self.indexed = false;

        hash_fn
    }

    /// Set a Rust function taking a reference to the scripting [`Engine`][crate::Engine],
    /// the current set of functions, plus a list of mutable [`Dynamic`] references
    /// into the [`Module`], returning a hash key.
    ///
    /// Use this to register a built-in function which must reference settings on the scripting
    /// [`Engine`][crate::Engine] (e.g. to prevent growing an array beyond the allowed maximum size),
    /// or to call a script-defined function in the current evaluation context.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # WARNING - Low Level API
    ///
    /// This function is very low level.
    ///
    /// A list of [`TypeId`]'s is taken as the argument types.
    ///
    /// Arguments are simply passed in as a mutable array of [`&mut Dynamic`][Dynamic],
    /// which is guaranteed to contain enough arguments of the correct types.
    ///
    /// The function is assumed to be a _method_, meaning that the first argument should not be consumed.
    /// All other arguments can be consumed.
    ///
    /// To access a primary parameter value (i.e. cloning is cheap), use: `args[n].clone().cast::<T>()`
    ///
    /// To access a parameter value and avoid cloning, use `std::mem::take(args[n]).cast::<T>()`.
    /// Notice that this will _consume_ the argument, replacing it with `()`.
    ///
    /// To access the first mutable parameter, use `args.get_mut(0).unwrap()`
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, FnNamespace, FnAccess};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_raw_fn("double_or_not", FnNamespace::Internal, FnAccess::Public,
    ///                 // Pass parameter types via a slice with TypeId's
    ///                 &[std::any::TypeId::of::<i64>(), std::any::TypeId::of::<bool>()],
    ///                 // Fixed closure signature
    ///                 |context, args| {
    ///                     // 'args' is guaranteed to be the right length and of the correct types
    ///
    ///                     // Get the second parameter by 'consuming' it
    ///                     let double = std::mem::take(args[1]).cast::<bool>();
    ///                     // Since it is a primary type, it can also be cheaply copied
    ///                     let double = args[1].clone().cast::<bool>();
    ///                     // Get a mutable reference to the first argument.
    ///                     let mut x = args[0].write_lock::<i64>().unwrap();
    ///
    ///                     let orig = *x;
    ///
    ///                     if double {
    ///                         *x *= 2;            // the first argument can be mutated
    ///                     }
    ///
    ///                     Ok(orig)                // return Result<T, Box<EvalAltResult>>
    ///                 });
    ///
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_raw_fn<T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        access: FnAccess,
        arg_types: &[TypeId],
        func: impl Fn(NativeCallContext, &mut FnCallArgs) -> Result<T, Box<EvalAltResult>>
            + SendSync
            + 'static,
    ) -> NonZeroU64 {
        let f =
            move |ctx: NativeCallContext, args: &mut FnCallArgs| func(ctx, args).map(Dynamic::from);

        self.set_fn(
            name,
            namespace,
            access,
            None,
            arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Set a Rust function taking no parameters into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_0("calc", || Ok(42_i64));
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_0<T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        func: impl Fn() -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, _: &mut FnCallArgs| func().map(Dynamic::from);
        let arg_types = [];
        self.set_fn(
            name,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_pure(Box::new(f)),
        )
    }

    /// Set a Rust function taking one parameter into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_1("calc", |x: i64| Ok(x + 1));
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_1<A: Variant + Clone, T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(A) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            func(cast_arg::<A>(&mut args[0])).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>()];
        self.set_fn(
            name,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_pure(Box::new(f)),
        )
    }

    /// Set a Rust function taking one mutable parameter into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, FnNamespace};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_1_mut("calc", FnNamespace::Internal,
    ///                 |x: &mut i64| { *x += 1; Ok(*x) }
    ///            );
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_1_mut<A: Variant + Clone, T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        func: impl Fn(&mut A) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            func(&mut args[0].write_lock::<A>().unwrap()).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>()];
        self.set_fn(
            name,
            namespace,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Set a Rust getter function taking one mutable parameter, returning a hash key.
    /// This function is automatically exposed to the global namespace.
    ///
    /// If there is a similar existing Rust getter function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::Module;
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_getter_fn("value", |x: &mut i64| { Ok(*x) });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[cfg(not(feature = "no_object"))]
    #[inline(always)]
    pub fn set_getter_fn<A: Variant + Clone, T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(&mut A) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        self.set_fn_1_mut(
            crate::engine::make_getter(&name.into()),
            FnNamespace::Global,
            func,
        )
    }

    /// Set a Rust function taking two parameters into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_2("calc", |x: i64, y: ImmutableString| {
    ///     Ok(x + y.len() as i64)
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_2<A: Variant + Clone, B: Variant + Clone, T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(A, B) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let a = cast_arg::<A>(&mut args[0]);
            let b = cast_arg::<B>(&mut args[1]);

            func(a, b).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>(), TypeId::of::<B>()];
        self.set_fn(
            name,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_pure(Box::new(f)),
        )
    }

    /// Set a Rust function taking two parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, FnNamespace, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_2_mut("calc", FnNamespace::Internal,
    ///                 |x: &mut i64, y: ImmutableString| {
    ///                     *x += y.len() as i64;
    ///                     Ok(*x)
    ///                 }
    ///            );
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_2_mut<A: Variant + Clone, B: Variant + Clone, T: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        func: impl Fn(&mut A, B) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let b = cast_arg::<B>(&mut args[1]);
            let a = &mut args[0].write_lock::<A>().unwrap();

            func(a, b).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>(), TypeId::of::<B>()];
        self.set_fn(
            name,
            namespace,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Set a Rust setter function taking two parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    /// This function is automatically exposed to the global namespace.
    ///
    /// If there is a similar existing setter Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_setter_fn("value", |x: &mut i64, y: ImmutableString| {
    ///     *x = y.len() as i64;
    ///     Ok(())
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[cfg(not(feature = "no_object"))]
    #[inline(always)]
    pub fn set_setter_fn<A: Variant + Clone, B: Variant + Clone>(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(&mut A, B) -> Result<(), Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        self.set_fn_2_mut(
            crate::engine::make_setter(&name.into()),
            FnNamespace::Global,
            func,
        )
    }

    /// Set a Rust index getter taking two parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    /// This function is automatically exposed to the global namespace.
    ///
    /// If there is a similar existing setter Rust function, it is replaced.
    ///
    /// # Panics
    ///
    /// Panics if the type is [`Array`] or [`Map`].
    /// Indexers for arrays, object maps and strings cannot be registered.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_indexer_get_fn(|x: &mut i64, y: ImmutableString| {
    ///     Ok(*x + y.len() as i64)
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[cfg(not(feature = "no_index"))]
    #[inline(always)]
    pub fn set_indexer_get_fn<A: Variant + Clone, B: Variant + Clone, T: Variant + Clone>(
        &mut self,
        func: impl Fn(&mut A, B) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        if TypeId::of::<A>() == TypeId::of::<Array>() {
            panic!("Cannot register indexer for arrays.");
        }
        #[cfg(not(feature = "no_object"))]
        if TypeId::of::<A>() == TypeId::of::<Map>() {
            panic!("Cannot register indexer for object maps.");
        }
        if TypeId::of::<A>() == TypeId::of::<String>()
            || TypeId::of::<A>() == TypeId::of::<&str>()
            || TypeId::of::<A>() == TypeId::of::<ImmutableString>()
        {
            panic!("Cannot register indexer for strings.");
        }

        self.set_fn_2_mut(crate::engine::FN_IDX_GET, FnNamespace::Global, func)
    }

    /// Set a Rust function taking three parameters into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_3("calc", |x: i64, y: ImmutableString, z: i64| {
    ///     Ok(x + y.len() as i64 + z)
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_3<
        A: Variant + Clone,
        B: Variant + Clone,
        C: Variant + Clone,
        T: Variant + Clone,
    >(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(A, B, C) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let a = cast_arg::<A>(&mut args[0]);
            let b = cast_arg::<B>(&mut args[1]);
            let c = cast_arg::<C>(&mut args[2]);

            func(a, b, c).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>(), TypeId::of::<B>(), TypeId::of::<C>()];
        self.set_fn(
            name,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_pure(Box::new(f)),
        )
    }

    /// Set a Rust function taking three parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, FnNamespace, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_3_mut("calc", FnNamespace::Internal,
    ///                 |x: &mut i64, y: ImmutableString, z: i64| {
    ///                     *x += y.len() as i64 + z;
    ///                     Ok(*x)
    ///                 }
    ///            );
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_3_mut<
        A: Variant + Clone,
        B: Variant + Clone,
        C: Variant + Clone,
        T: Variant + Clone,
    >(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        func: impl Fn(&mut A, B, C) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let b = cast_arg::<B>(&mut args[2]);
            let c = cast_arg::<C>(&mut args[3]);
            let a = &mut args[0].write_lock::<A>().unwrap();

            func(a, b, c).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>(), TypeId::of::<B>(), TypeId::of::<C>()];
        self.set_fn(
            name,
            namespace,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Set a Rust index setter taking three parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    /// This function is automatically exposed to the global namespace.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Panics
    ///
    /// Panics if the type is [`Array`] or [`Map`].
    /// Indexers for arrays, object maps and strings cannot be registered.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_indexer_set_fn(|x: &mut i64, y: ImmutableString, value: i64| {
    ///     *x = y.len() as i64 + value;
    ///     Ok(())
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[cfg(not(feature = "no_index"))]
    #[inline(always)]
    pub fn set_indexer_set_fn<A: Variant + Clone, B: Variant + Clone, C: Variant + Clone>(
        &mut self,
        func: impl Fn(&mut A, B, C) -> Result<(), Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        if TypeId::of::<A>() == TypeId::of::<Array>() {
            panic!("Cannot register indexer for arrays.");
        }
        #[cfg(not(feature = "no_object"))]
        if TypeId::of::<A>() == TypeId::of::<Map>() {
            panic!("Cannot register indexer for object maps.");
        }
        if TypeId::of::<A>() == TypeId::of::<String>()
            || TypeId::of::<A>() == TypeId::of::<&str>()
            || TypeId::of::<A>() == TypeId::of::<ImmutableString>()
        {
            panic!("Cannot register indexer for strings.");
        }

        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let b = cast_arg::<B>(&mut args[1]);
            let c = cast_arg::<C>(&mut args[2]);
            let a = &mut args[0].write_lock::<A>().unwrap();

            func(a, b, c).map(Dynamic::from)
        };
        let arg_types = [TypeId::of::<A>(), TypeId::of::<B>(), TypeId::of::<C>()];
        self.set_fn(
            crate::engine::FN_IDX_SET,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Set a pair of Rust index getter and setter functions, returning both hash keys.
    /// This is a short-hand for [`set_indexer_get_fn`][Module::set_indexer_get_fn] and
    /// [`set_indexer_set_fn`][Module::set_indexer_set_fn].
    ///
    /// If there are similar existing Rust functions, they are replaced.
    ///
    /// # Panics
    ///
    /// Panics if the type is [`Array`] or [`Map`].
    /// Indexers for arrays, object maps and strings cannot be registered.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let (hash_get, hash_set) = module.set_indexer_get_set_fn(
    ///     |x: &mut i64, y: ImmutableString| {
    ///         Ok(*x + y.len() as i64)
    ///     },
    ///     |x: &mut i64, y: ImmutableString, value: i64| {
    ///         *x = y.len() as i64 + value;
    ///         Ok(())
    ///     }
    /// );
    /// assert!(module.contains_fn(hash_get, true));
    /// assert!(module.contains_fn(hash_set, true));
    /// ```
    #[cfg(not(feature = "no_index"))]
    #[inline(always)]
    pub fn set_indexer_get_set_fn<A: Variant + Clone, B: Variant + Clone, T: Variant + Clone>(
        &mut self,
        getter: impl Fn(&mut A, B) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
        setter: impl Fn(&mut A, B, T) -> Result<(), Box<EvalAltResult>> + SendSync + 'static,
    ) -> (NonZeroU64, NonZeroU64) {
        (
            self.set_indexer_get_fn(getter),
            self.set_indexer_set_fn(setter),
        )
    }

    /// Set a Rust function taking four parameters into the [`Module`], returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_4("calc", |x: i64, y: ImmutableString, z: i64, _w: ()| {
    ///     Ok(x + y.len() as i64 + z)
    /// });
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_4<
        A: Variant + Clone,
        B: Variant + Clone,
        C: Variant + Clone,
        D: Variant + Clone,
        T: Variant + Clone,
    >(
        &mut self,
        name: impl Into<String>,
        func: impl Fn(A, B, C, D) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let a = cast_arg::<A>(&mut args[0]);
            let b = cast_arg::<B>(&mut args[1]);
            let c = cast_arg::<C>(&mut args[2]);
            let d = cast_arg::<D>(&mut args[3]);

            func(a, b, c, d).map(Dynamic::from)
        };
        let arg_types = [
            TypeId::of::<A>(),
            TypeId::of::<B>(),
            TypeId::of::<C>(),
            TypeId::of::<D>(),
        ];
        self.set_fn(
            name,
            FnNamespace::Internal,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_pure(Box::new(f)),
        )
    }

    /// Set a Rust function taking four parameters (the first one mutable) into the [`Module`],
    /// returning a hash key.
    ///
    /// If there is a similar existing Rust function, it is replaced.
    ///
    /// # Function Metadata
    ///
    /// No metadata for the function is registered. Use `update_fn_metadata` to add metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Module, FnNamespace, ImmutableString};
    ///
    /// let mut module = Module::new();
    /// let hash = module.set_fn_4_mut("calc", FnNamespace::Internal,
    ///                 |x: &mut i64, y: ImmutableString, z: i64, _w: ()| {
    ///                     *x += y.len() as i64 + z;
    ///                     Ok(*x)
    ///                 }
    ///            );
    /// assert!(module.contains_fn(hash, true));
    /// ```
    #[inline(always)]
    pub fn set_fn_4_mut<
        A: Variant + Clone,
        B: Variant + Clone,
        C: Variant + Clone,
        D: Variant + Clone,
        T: Variant + Clone,
    >(
        &mut self,
        name: impl Into<String>,
        namespace: FnNamespace,
        func: impl Fn(&mut A, B, C, D) -> Result<T, Box<EvalAltResult>> + SendSync + 'static,
    ) -> NonZeroU64 {
        let f = move |_: NativeCallContext, args: &mut FnCallArgs| {
            let b = cast_arg::<B>(&mut args[1]);
            let c = cast_arg::<C>(&mut args[2]);
            let d = cast_arg::<D>(&mut args[3]);
            let a = &mut args[0].write_lock::<A>().unwrap();

            func(a, b, c, d).map(Dynamic::from)
        };
        let arg_types = [
            TypeId::of::<A>(),
            TypeId::of::<B>(),
            TypeId::of::<C>(),
            TypeId::of::<D>(),
        ];
        self.set_fn(
            name,
            namespace,
            FnAccess::Public,
            None,
            &arg_types,
            CallableFunction::from_method(Box::new(f)),
        )
    }

    /// Get a Rust function.
    ///
    /// The [`NonZeroU64`] hash is calculated by the function [`calc_native_fn_hash`][crate::calc_native_fn_hash].
    /// It is also returned by the `set_fn_XXX` calls.
    #[inline(always)]
    pub(crate) fn get_fn(
        &self,
        hash_fn: NonZeroU64,
        public_only: bool,
    ) -> Option<&CallableFunction> {
        self.functions
            .get(&hash_fn)
            .and_then(|FuncInfo { access, func, .. }| match access {
                _ if !public_only => Some(func),
                FnAccess::Public => Some(func),
                FnAccess::Private => None,
            })
    }

    /// Does the particular namespace-qualified function exist in the [`Module`]?
    ///
    /// The [`NonZeroU64`] hash is calculated by the function
    /// [`calc_native_fn_hash`][crate::calc_native_fn_hash] and must match
    /// the hash calculated by [`build_index`][Module::build_index].
    #[inline(always)]
    pub fn contains_qualified_fn(&self, hash_fn: NonZeroU64) -> bool {
        self.all_functions.contains_key(&hash_fn)
    }

    /// Get a namespace-qualified function.
    ///
    /// The [`NonZeroU64`] hash is calculated by the function
    /// [`calc_native_fn_hash`][crate::calc_native_fn_hash] and must match
    /// the hash calculated by [`build_index`][Module::build_index].
    #[inline(always)]
    pub(crate) fn get_qualified_fn(
        &self,
        hash_qualified_fn: NonZeroU64,
    ) -> Option<&CallableFunction> {
        self.all_functions.get(&hash_qualified_fn)
    }

    /// Combine another [`Module`] into this [`Module`].
    /// The other [`Module`] is _consumed_ to merge into this [`Module`].
    #[inline]
    pub fn combine(&mut self, other: Self) -> &mut Self {
        self.modules.extend(other.modules.into_iter());
        self.variables.extend(other.variables.into_iter());
        self.functions.extend(other.functions.into_iter());
        self.type_iterators.extend(other.type_iterators.into_iter());
        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;
        self
    }

    /// Combine another [`Module`] into this [`Module`].
    /// The other [`Module`] is _consumed_ to merge into this [`Module`].
    /// Sub-modules are flattened onto the root [`Module`], with higher level overriding lower level.
    #[inline]
    pub fn combine_flatten(&mut self, other: Self) -> &mut Self {
        other.modules.into_iter().for_each(|(_, m)| {
            self.combine_flatten(shared_take_or_clone(m));
        });
        self.variables.extend(other.variables.into_iter());
        self.functions.extend(other.functions.into_iter());
        self.type_iterators.extend(other.type_iterators.into_iter());
        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;
        self
    }

    /// Polyfill this [`Module`] with another [`Module`].
    /// Only items not existing in this [`Module`] are added.
    #[inline]
    pub fn fill_with(&mut self, other: &Self) -> &mut Self {
        other.modules.iter().for_each(|(k, v)| {
            if !self.modules.contains_key(k) {
                self.modules.insert(k.clone(), v.clone());
            }
        });
        other.variables.iter().for_each(|(k, v)| {
            if !self.variables.contains_key(k) {
                self.variables.insert(k.clone(), v.clone());
            }
        });
        other.functions.iter().for_each(|(&k, v)| {
            self.functions.entry(k).or_insert_with(|| v.clone());
        });
        other.type_iterators.iter().for_each(|(&k, &v)| {
            self.type_iterators.entry(k).or_insert(v);
        });
        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;
        self
    }

    /// Merge another [`Module`] into this [`Module`].
    #[inline(always)]
    pub fn merge(&mut self, other: &Self) -> &mut Self {
        self.merge_filtered(other, &mut |_, _, _, _, _| true)
    }

    /// Merge another [`Module`] into this [`Module`] based on a filter predicate.
    pub(crate) fn merge_filtered(
        &mut self,
        other: &Self,
        mut _filter: &mut impl FnMut(FnNamespace, FnAccess, bool, &str, usize) -> bool,
    ) -> &mut Self {
        #[cfg(not(feature = "no_function"))]
        other.modules.iter().for_each(|(k, v)| {
            let mut m = Self::new();
            m.merge_filtered(v, _filter);
            self.set_sub_module(k.clone(), m);
        });
        #[cfg(feature = "no_function")]
        self.modules
            .extend(other.modules.iter().map(|(k, v)| (k.clone(), v.clone())));

        self.variables
            .extend(other.variables.iter().map(|(k, v)| (k.clone(), v.clone())));
        self.functions.extend(
            other
                .functions
                .iter()
                .filter(
                    |(
                        _,
                        FuncInfo {
                            namespace,
                            access,
                            name,
                            params,
                            func,
                            ..
                        },
                    )| {
                        _filter(
                            *namespace,
                            *access,
                            func.is_script(),
                            name.as_str(),
                            *params,
                        )
                    },
                )
                .map(|(&k, v)| (k, v.clone())),
        );

        self.type_iterators.extend(other.type_iterators.iter());
        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;
        self
    }

    /// Filter out the functions, retaining only some script-defined functions based on a filter predicate.
    #[cfg(not(feature = "no_function"))]
    #[inline]
    pub(crate) fn retain_script_functions(
        &mut self,
        mut filter: impl FnMut(FnNamespace, FnAccess, &str, usize) -> bool,
    ) -> &mut Self {
        self.functions.retain(
            |_,
             FuncInfo {
                 namespace,
                 access,
                 name,
                 params,
                 func,
                 ..
             }| {
                if func.is_script() {
                    filter(*namespace, *access, name.as_str(), *params)
                } else {
                    false
                }
            },
        );

        self.all_functions.clear();
        self.all_variables.clear();
        self.all_type_iterators.clear();
        self.indexed = false;
        self
    }

    /// Get the number of variables, functions and type iterators in the [`Module`].
    #[inline(always)]
    pub fn count(&self) -> (usize, usize, usize) {
        (
            self.variables.len(),
            self.functions.len(),
            self.type_iterators.len(),
        )
    }

    /// Get an iterator to the sub-modules in the [`Module`].
    #[inline(always)]
    pub fn iter_sub_modules(&self) -> impl Iterator<Item = (&str, Shared<Module>)> {
        self.modules.iter().map(|(k, m)| (k.as_str(), m.clone()))
    }

    /// Get an iterator to the variables in the [`Module`].
    #[inline(always)]
    pub fn iter_var(&self) -> impl Iterator<Item = (&str, &Dynamic)> {
        self.variables.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Get an iterator to the functions in the [`Module`].
    #[cfg(not(feature = "no_optimize"))]
    #[cfg(not(feature = "no_function"))]
    #[inline(always)]
    pub(crate) fn iter_fn(&self) -> impl Iterator<Item = &FuncInfo> {
        self.functions.values()
    }

    /// Get an iterator over all script-defined functions in the [`Module`].
    ///
    /// Function metadata includes:
    /// 1) Namespace ([`FnNamespace::Global`] or [`FnNamespace::Internal`]).
    /// 2) Access mode ([`FnAccess::Public`] or [`FnAccess::Private`]).
    /// 3) Function name (as string slice).
    /// 4) Number of parameters.
    /// 5) Shared reference to function definition [`ScriptFnDef`][crate::ast::ScriptFnDef].
    #[cfg(not(feature = "no_function"))]
    #[inline(always)]
    pub(crate) fn iter_script_fn<'a>(
        &'a self,
    ) -> impl Iterator<Item = (FnNamespace, FnAccess, &str, usize, &ScriptFnDef)> + 'a {
        self.functions.values().filter(|f| f.func.is_script()).map(
            |FuncInfo {
                 namespace,
                 access,
                 name,
                 params,
                 func,
                 ..
             }| {
                (
                    *namespace,
                    *access,
                    name.as_str(),
                    *params,
                    func.get_fn_def(),
                )
            },
        )
    }

    /// Get an iterator over all script-defined functions in the [`Module`].
    ///
    /// Function metadata includes:
    /// 1) Namespace ([`FnNamespace::Global`] or [`FnNamespace::Internal`]).
    /// 2) Access mode ([`FnAccess::Public`] or [`FnAccess::Private`]).
    /// 3) Function name (as string slice).
    /// 4) Number of parameters.
    #[cfg(not(feature = "no_function"))]
    #[cfg(not(feature = "internals"))]
    #[inline(always)]
    pub fn iter_script_fn_info(
        &self,
    ) -> impl Iterator<Item = (FnNamespace, FnAccess, &str, usize)> {
        self.functions.values().filter(|f| f.func.is_script()).map(
            |FuncInfo {
                 name,
                 namespace,
                 access,
                 params,
                 ..
             }| (*namespace, *access, name.as_str(), *params),
        )
    }

    /// Get an iterator over all script-defined functions in the [`Module`].
    ///
    /// Function metadata includes:
    /// 1) Namespace ([`FnNamespace::Global`] or [`FnNamespace::Internal`]).
    /// 2) Access mode ([`FnAccess::Public`] or [`FnAccess::Private`]).
    /// 3) Function name (as string slice).
    /// 4) Number of parameters.
    /// 5) _(INTERNALS)_ Shared reference to function definition [`ScriptFnDef`][crate::ast::ScriptFnDef].
    ///    Exported under the `internals` feature only.
    #[cfg(not(feature = "no_function"))]
    #[cfg(feature = "internals")]
    #[inline(always)]
    pub fn iter_script_fn_info(
        &self,
    ) -> impl Iterator<Item = (FnNamespace, FnAccess, &str, usize, &ScriptFnDef)> {
        self.iter_script_fn()
    }

    /// Create a new [`Module`] by evaluating an [`AST`][crate::AST].
    ///
    /// The entire [`AST`][crate::AST] is encapsulated into each function, allowing functions
    /// to cross-call each other.  Functions in the global namespace, plus all functions
    /// defined in the [`Module`], are _merged_ into a _unified_ namespace before each call.
    /// Therefore, all functions will be found.
    ///
    /// # Example
    ///
    /// ```
    /// # fn main() -> Result<(), Box<rhai::EvalAltResult>> {
    /// use rhai::{Engine, Module, Scope};
    ///
    /// let engine = Engine::new();
    /// let ast = engine.compile("let answer = 42; export answer;")?;
    /// let module = Module::eval_ast_as_new(Scope::new(), &ast, &engine)?;
    /// assert!(module.contains_var("answer"));
    /// assert_eq!(module.get_var_value::<i64>("answer").unwrap(), 42);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(not(feature = "no_module"))]
    pub fn eval_ast_as_new(
        mut scope: crate::Scope,
        ast: &crate::AST,
        engine: &crate::Engine,
    ) -> Result<Self, Box<EvalAltResult>> {
        let mut mods: crate::engine::Imports = (&engine.global_sub_modules).into();
        let orig_mods_len = mods.len();

        // Run the script
        engine.eval_ast_with_scope_raw(&mut scope, &mut mods, &ast, 0)?;

        // Create new module
        let mut module = Module::new();

        scope.into_iter().for_each(|(_, value, mut aliases)| {
            // Variables with an alias left in the scope become module variables
            if aliases.len() > 1 {
                aliases.into_iter().for_each(|alias| {
                    module.variables.insert(alias, value.clone());
                });
            } else if aliases.len() == 1 {
                module.variables.insert(aliases.pop().unwrap(), value);
            }
        });

        // Extra modules left in the scope become sub-modules
        let mut func_mods: crate::engine::Imports = Default::default();

        mods.into_iter().skip(orig_mods_len).for_each(|(alias, m)| {
            func_mods.push(alias.clone(), m.clone());
            module.set_sub_module(alias, m);
        });

        // Non-private functions defined become module functions
        #[cfg(not(feature = "no_function"))]
        {
            ast.lib()
                .functions
                .values()
                .filter(|FuncInfo { access, func, .. }| !access.is_private() && func.is_script())
                .for_each(|FuncInfo { func, .. }| {
                    // Encapsulate AST environment
                    let mut func = func.get_fn_def().clone();
                    func.lib = Some(ast.shared_lib());
                    func.mods = func_mods.clone();
                    module.set_script_fn(func);
                });
        }

        module.set_id(ast.clone_source());
        module.build_index();

        Ok(module)
    }

    /// Scan through all the sub-modules in the [`Module`] and build a hash index of all
    /// variables and functions as one flattened namespace.
    ///
    /// If the [`Module`] is already indexed, this method has no effect.
    pub fn build_index(&mut self) -> &mut Self {
        // Collect a particular module.
        fn index_module<'a>(
            module: &'a Module,
            qualifiers: &mut Vec<&'a str>,
            variables: &mut HashMap<NonZeroU64, Dynamic, StraightHasherBuilder>,
            functions: &mut HashMap<NonZeroU64, CallableFunction, StraightHasherBuilder>,
            type_iterators: &mut HashMap<TypeId, IteratorFn>,
        ) {
            module.modules.iter().for_each(|(name, m)| {
                // Index all the sub-modules first.
                qualifiers.push(name);
                index_module(m, qualifiers, variables, functions, type_iterators);
                qualifiers.pop();
            });

            // Index all variables
            module.variables.iter().for_each(|(var_name, value)| {
                // Qualifiers + variable name
                let hash_var =
                    crate::calc_script_fn_hash(qualifiers.iter().map(|&v| v), var_name, 0).unwrap();
                variables.insert(hash_var, value.clone());
            });

            // Index type iterators
            module.type_iterators.iter().for_each(|(&type_id, func)| {
                type_iterators.insert(type_id, func.clone());
            });

            // Index all Rust functions
            module
                .functions
                .iter()
                .filter(|(_, FuncInfo { access, .. })| access.is_public())
                .for_each(
                    |(
                        &hash,
                        FuncInfo {
                            name,
                            namespace,
                            params,
                            param_types,
                            func,
                            ..
                        },
                    )| {
                        // Flatten all functions with global namespace
                        if namespace.is_global() {
                            functions.insert(hash, func.clone());
                        }

                        // Qualifiers + function name + number of arguments.
                        let hash_qualified_script =
                            crate::calc_script_fn_hash(qualifiers.iter().cloned(), name, *params)
                                .unwrap();

                        if !func.is_script() {
                            assert_eq!(*params, param_types.len());

                            // Namespace-qualified Rust functions are indexed in two steps:
                            // 1) Calculate a hash in a similar manner to script-defined functions,
                            //    i.e. qualifiers + function name + number of arguments.
                            // 2) Calculate a second hash with no qualifiers, empty function name,
                            //    and the actual list of argument [`TypeId`]'.s
                            let hash_fn_args = crate::calc_native_fn_hash(
                                empty(),
                                "",
                                param_types.iter().cloned(),
                            )
                            .unwrap();
                            // 3) The two hashes are combined.
                            let hash_qualified_fn =
                                combine_hashes(hash_qualified_script, hash_fn_args);

                            functions.insert(hash_qualified_fn, func.clone());
                        } else if cfg!(not(feature = "no_function")) {
                            functions.insert(hash_qualified_script, func.clone());
                        }
                    },
                );
        }

        if !self.indexed {
            let mut qualifiers = Vec::with_capacity(4);
            let mut variables = HashMap::with_capacity_and_hasher(16, StraightHasherBuilder);
            let mut functions = HashMap::with_capacity_and_hasher(256, StraightHasherBuilder);
            let mut type_iterators = HashMap::with_capacity(16);

            qualifiers.push("root");

            index_module(
                self,
                &mut qualifiers,
                &mut variables,
                &mut functions,
                &mut type_iterators,
            );

            self.all_variables = variables;
            self.all_functions = functions;
            self.all_type_iterators = type_iterators;
            self.indexed = true;
        }

        self
    }

    /// Does a type iterator exist in the entire module tree?
    pub fn contains_qualified_iter(&self, id: TypeId) -> bool {
        self.all_type_iterators.contains_key(&id)
    }

    /// Does a type iterator exist in the module?
    pub fn contains_iter(&self, id: TypeId) -> bool {
        self.type_iterators.contains_key(&id)
    }

    /// Set a type iterator into the [`Module`].
    pub fn set_iter(&mut self, typ: TypeId, func: IteratorFn) -> &mut Self {
        self.type_iterators.insert(typ, func);
        self.indexed = false;
        self
    }

    /// Set a type iterator into the [`Module`].
    pub fn set_iterable<T>(&mut self) -> &mut Self
    where
        T: Variant + Clone + IntoIterator,
        <T as IntoIterator>::Item: Variant + Clone,
    {
        self.set_iter(TypeId::of::<T>(), |obj: Dynamic| {
            Box::new(obj.cast::<T>().into_iter().map(Dynamic::from))
        })
    }

    /// Set an iterator type into the [`Module`] as a type iterator.
    pub fn set_iterator<T>(&mut self) -> &mut Self
    where
        T: Variant + Clone + Iterator,
        <T as Iterator>::Item: Variant + Clone,
    {
        self.set_iter(TypeId::of::<T>(), |obj: Dynamic| {
            Box::new(obj.cast::<T>().map(Dynamic::from))
        })
    }

    /// Get the specified type iterator.
    pub(crate) fn get_qualified_iter(&self, id: TypeId) -> Option<IteratorFn> {
        self.all_type_iterators.get(&id).cloned()
    }

    /// Get the specified type iterator.
    pub(crate) fn get_iter(&self, id: TypeId) -> Option<IteratorFn> {
        self.type_iterators.get(&id).cloned()
    }
}

/// _(INTERNALS)_ A chain of [module][Module] names to namespace-qualify a variable or function call.
/// Exported under the `internals` feature only.
///
/// A [`NonZeroU64`] offset to the current [`Scope`][crate::Scope] is cached for quick search purposes.
///
/// A [`StaticVec`] is used because most namespace-qualified access contains only one level,
/// and it is wasteful to always allocate a [`Vec`] with one element.
///
/// # WARNING
///
/// This type is volatile and may change.
#[derive(Clone, Eq, PartialEq, Default, Hash)]
pub struct NamespaceRef(Option<NonZeroUsize>, StaticVec<Ident>);

impl fmt::Debug for NamespaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.1, f)?;

        if let Some(index) = self.0 {
            write!(f, " -> {}", index)
        } else {
            Ok(())
        }
    }
}

impl Deref for NamespaceRef {
    type Target = StaticVec<Ident>;

    fn deref(&self) -> &Self::Target {
        &self.1
    }
}

impl DerefMut for NamespaceRef {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.1
    }
}

impl fmt::Display for NamespaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for Ident { name, .. } in self.1.iter() {
            write!(f, "{}{}", name, Token::DoubleColon.syntax())?;
        }
        Ok(())
    }
}

impl From<StaticVec<Ident>> for NamespaceRef {
    fn from(modules: StaticVec<Ident>) -> Self {
        Self(None, modules)
    }
}

impl<M: AsRef<Module>> Add<M> for &Module {
    type Output = Module;

    fn add(self, rhs: M) -> Self::Output {
        let mut module = self.clone();
        module.merge(rhs.as_ref());
        module
    }
}

impl<M: AsRef<Module>> Add<M> for Module {
    type Output = Self;

    fn add(mut self, rhs: M) -> Self::Output {
        self.merge(rhs.as_ref());
        self
    }
}

impl<M: Into<Module>> AddAssign<M> for Module {
    fn add_assign(&mut self, rhs: M) {
        self.combine(rhs.into());
    }
}

impl NamespaceRef {
    /// Get the [`Scope`][crate::Scope] index offset.
    pub(crate) fn index(&self) -> Option<NonZeroUsize> {
        self.0
    }
    /// Set the [`Scope`][crate::Scope] index offset.
    #[cfg(not(feature = "no_module"))]
    pub(crate) fn set_index(&mut self, index: Option<NonZeroUsize>) {
        self.0 = index
    }
}

#[cfg(not(feature = "no_module"))]
pub use resolvers::ModuleResolver;

/// Module containing all built-in [module resolvers][ModuleResolver].
#[cfg(not(feature = "no_module"))]
pub mod resolvers;
